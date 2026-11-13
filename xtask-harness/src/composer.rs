// SPDX-License-Identifier: MIT
//! `xtask test composer`: the in-app SurfaceFlinger (crate atl-composer) against AOSP's libgui in a real app process (M3 slice 4).
//! atld starts the services and the composer fixture's class (xtask-harness/testdata/composer: `Client`) as an app process (`--run-class`, uid 10123): its launcher starts the process's compositor, served as
//! `SurfaceFlinger` / `SurfaceFlingerAIDL` by the process's own service view (atl-service-view), where libgui's unpatched lookup finds it; its UI agent is registered through the system process. The client then does what a window does with the framework's public API:
//! two layers (`createConnection`, `createSurface`), 30 transactions (`setTransactionState`), a transaction-committed callback (the compositor's call into libgui's TransactionCompletedListener) and Choreographer's vsync (`createDisplayEventConnection`, the BitTube).
//! Pass (all checked from the logs, [`evaluate`]): both services registered; every client step logged; the compositor's own counters (its last `composer: counters` line) show every transaction applied, both layers created and gone, every vsync event sent and
//! none dropped, the callback delivered; the vsync cadence is the display model's refresh interval (Choreographer's frame times) and close to it on the wall clock; no FATAL EXCEPTION and no wait on an unserved service.
//! Buffers (the allocator HAL runs under atld, `Runtime::with_gralloc`): the client posts 30 frames through a BLASTBufferQueue, a software Canvas drawing a pattern of four quadrants whose colours are a formula of the frame number (Client.java).
//! The compositor's Scene sink reads every quad's dma-buf and reports its digest (atl-composer `ReportSink`); the gate computes the same pictures from the formula ([`expected_pixels`], independent of both) and requires every shown buffer
//! to be one of them, in order, 20 or more distinct, the last frame shown; no `lockCanvas` stall; every acquired buffer released, through the layer's BufferReleaseChannel (a release to a queue that is already gone is counted apart);
//! and the descriptors back to where they were: the compositor holds none once the layers are gone, and the client's own count of open descriptors is the baseline it took after a warm-up cycle.
//! `--self-test` checks [`evaluate`] on synthetic logs: a good one passes, each defect is caught.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use atl_layout::Layout;
use atl_services_types::display_model::DisplayModel;

use crate::atld_run::{Package, Run, Runtime, Shell, counter_lines, run_display};
use crate::manifest;
use crate::verdict::{Failure, Verdict};

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
    /// Instead of the run: the verdict on synthetic logs (what the gate must pass and must fail). Needs no device.
    #[arg(long)]
    self_test: bool,
    /// Deliberate failure: the compositor is not served by the process's service view (atl-composer `BREAK_NO_SERVE`); the run must fail, libgui left waiting for `SurfaceFlinger`.
    #[arg(long)]
    break_view: bool,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole client log.
    #[arg(long, short)]
    verbose: bool,
}

/// The fixture's dir, its client class and process name.
const FIXTURE: &str = "xtask-harness/testdata/composer";
const CLIENT_CLASS: &str = "com.example.atlcomposer.Client";
const CLIENT_NAME: &str = "composer-client";
/// The client's uid: AID_APP_START + 123, any app id (as `xtask test app`).
const APP_UID: u32 = 10_123;
/// How long the cold start and the client may take (as `xtask test app`).
const LIMIT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(50);
/// Lines shown of a log on failure.
const TAIL: usize = 40;
/// The vsync frames the client observes and posts (Client.java FRAMES).
const FRAMES: u64 = 30;
/// Distinct frames the Scene sink must have shown: 20 or more of the 30 (a tick shows the newest buffer, so a fast producer's frames can be skipped, never reordered).
const MIN_SHOWN_FRAMES: usize = 20;
/// The buffer's side in pixels (Client.java SIZE) and the layer names of the warm-up and the run (Client.java).
const SIZE: u32 = 64;
const WARM_LAYER: &str = "atl-warm-child";
const MAIN_LAYER: &str = "atl-child";
/// The longest `lockCanvas` may take, ms. A BLASTBufferQueue that is never released blocks dequeueBuffer for its 3 s timeout; a healthy one dequeues in about a millisecond (500 ms leaves room for a loaded machine and first-use allocation).
const MAX_LOCK_MS: u64 = 500;
/// Choreographer's mean frame interval may differ from the display's interval by this fraction: its times are the compositor's grid, exact up to the rounding of the interval to a nanosecond and of the mean to a microsecond.
const FRAME_INTERVAL_TOLERANCE: f64 = 0.01;
/// The wall-clock mean may differ by this fraction: sleeping to a deadline on a loaded machine is not exact, and thirty frames average the jitter, not remove it (2026-11-04: set from reading, not measured; tighten once a run shows the band).
const WALL_INTERVAL_TOLERANCE: f64 = 0.25;

/// One checked fact.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        ok,
        detail: detail.into(),
    }
}

/// `key=value` of a `composer: counters` line.
pub(crate) fn field(line: &str, key: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

/// The last `composer: counters` line: the compositor prints one each time its clock goes idle, so the last is the state after everything.
pub(crate) fn last_counters(app: &str) -> Option<&str> {
    app.lines()
        .rev()
        .find(|l| l.contains("composer: counters "))
}

/// A number after `key=` in an `AtlComposer:` client line.
fn client_value(app: &str, key: &str) -> Option<u64> {
    app.lines()
        .filter(|l| l.contains("AtlComposer:"))
        .find_map(|l| field(l, key))
}

/// Whether a client line says `text`.
fn client_says(app: &str, text: &str) -> bool {
    app.lines()
        .any(|l| l.contains("AtlComposer:") && l.contains(text))
}

/// FNV-1a, 64 bit (offset basis 0xcbf29ce484222325, prime 0x100000001b3): the digest the compositor reports a quad's pixels by (atl-composer sink.rs; both are checked against the published vectors).
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Frame `n`'s picture, from the formula in Client.java's header: (r, g, b) = (37 n, 91 n + 17, 255 - 5 n) mod 256; the quadrants top-left, top-right, bottom-left, bottom-right are (r, g, b), (g, b, r), (b, r, g) and
/// (255 - r, 255 - g, 255 - b); rows of [`SIZE`] RGBA pixels, alpha 255.
fn expected_pixels(n: u32) -> Vec<u8> {
    let (r, g, b) = (
        (37 * n) & 255,
        (91 * n + 17) & 255,
        255u32.wrapping_sub(5 * n) & 255,
    );
    let quadrants = [[r, g, b], [g, b, r], [b, r, g], [255 - r, 255 - g, 255 - b]];
    let half = SIZE / 2;
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let q = usize::from(x >= half) + 2 * usize::from(y >= half);
            pixels.extend(quadrants[q].iter().map(|&c| c as u8));
            pixels.push(255);
        }
    }
    pixels
}

fn expected_digest(n: u32) -> u64 {
    fnv1a64(&expected_pixels(n))
}

/// One `composer: quad` line of the compositor's report.
struct Quad {
    name: String,
    buffer: String,
    /// None: the sink could not read the buffer (`unreadable=`).
    digest: Option<u64>,
}

pub(crate) fn word<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

fn parse_quads(app: &str) -> Vec<Quad> {
    app.lines()
        .filter(|l| l.starts_with("composer: quad "))
        .map(|l| Quad {
            name: word(l, "name").unwrap_or_default().to_owned(),
            buffer: word(l, "buffer").unwrap_or_default().to_owned(),
            digest: word(l, "digest").and_then(|d| u64::from_str_radix(d, 16).ok()),
        })
        .collect()
}

/// The checks on what the Scene sink showed: every quad readable and the right size; the main layer's buffers each one of frames 1 to 30 in the picture the formula gives, never going back, [`MIN_SHOWN_FRAMES`] or more distinct, the last frame
/// among them at the end; the warm-up layer showing frame 0.
fn pixel_checks(app: &str) -> Vec<Check> {
    let quads = parse_quads(app);
    let size = format!("{SIZE}x{SIZE}");
    let readable =
        !quads.is_empty() && quads.iter().all(|q| q.digest.is_some() && q.buffer == size);
    let by_digest: HashMap<u64, u32> = (1..=FRAMES as u32)
        .map(|n| (expected_digest(n), n))
        .collect();
    let shown: Vec<Option<u32>> = quads
        .iter()
        .filter(|q| q.name == MAIN_LAYER)
        .map(|q| q.digest.and_then(|d| by_digest.get(&d).copied()))
        .collect();
    let frames: Vec<u32> = shown.iter().flatten().copied().collect();
    let distinct = frames.iter().collect::<BTreeSet<_>>().len();
    let ordered = frames.windows(2).all(|w| w[0] <= w[1]);
    let warm: Vec<&Quad> = quads.iter().filter(|q| q.name == WARM_LAYER).collect();
    let warm_ok = !warm.is_empty() && warm.iter().all(|q| q.digest == Some(expected_digest(0)));
    let pixels_ok = !shown.is_empty()
        && shown.iter().all(Option::is_some)
        && ordered
        && distinct >= MIN_SHOWN_FRAMES
        && frames.last() == Some(&(FRAMES as u32))
        && warm_ok;
    vec![
        check(
            "quads readable",
            readable,
            format!("{} quads, each {size} and readable", quads.len()),
        ),
        check(
            "pixels",
            pixels_ok,
            format!(
                "main layer showed frames {frames:?} ({} of {} quads match the formula), {distinct} distinct (at least {MIN_SHOWN_FRAMES}), ordered {ordered}, last {:?} (want {FRAMES}); warm-up frame 0 {warm_ok}",
                frames.len(),
                shown.len(),
                frames.last()
            ),
        ),
    ]
}

/// Every check of a run, from its client log (the process's stdout and stderr: liblog records and the compositor's stderr lines) and the services' log.
fn evaluate(app: &str, services: &str, display: &DisplayModel) -> Vec<Check> {
    let mut out = Vec::new();
    let served = served_line();
    let agent = agent_line();
    out.push(check(
        "registered",
        app.contains(&served) && services.contains(&agent),
        format!("`{served}` in the app, `{agent}` in the services"),
    ));
    let steps = [
        ("layers", "layers created valid=true"),
        ("bbq surface", "bbq created surface valid=true"),
        ("vsync complete", "complete=true"),
        ("callback fired", "committed callback fired=true"),
        ("client done", "AtlComposer: done"),
    ];
    for (label, text) in steps {
        out.push(check(label, client_says(app, text), format!("`{text}`")));
    }
    let Some(line) = last_counters(app) else {
        out.push(check(
            "compositor counters",
            false,
            "no `composer: counters` line",
        ));
        return finish(out, app);
    };
    let n = |key: &str| field(line, key);
    out.push(check(
        "transactions applied",
        n("transactions").is_some_and(|t| t >= FRAMES)
            && n("transactions") == n("transactions_applied"),
        format!(
            "transactions={:?} applied={:?}, at least {FRAMES} (one per vsync frame)",
            n("transactions"),
            n("transactions_applied")
        ),
    ));
    out.push(check(
        "layers",
        n("layers_created") == Some(4) && n("layers_alive") == Some(0) && n("unknown_layer") == Some(0),
        format!(
            "created={:?} alive={:?} states for unknown layers={:?}: four created (the warm-up's two and the run's), all released by the client",
            n("layers_created"),
            n("layers_alive"),
            n("unknown_layer")
        ),
    ));
    out.push(check(
        "vsync delivered",
        n("vsync_sent").is_some_and(|s| s >= FRAMES) && n("vsync_dropped") == Some(0),
        format!(
            "sent={:?} dropped={:?}",
            n("vsync_sent"),
            n("vsync_dropped")
        ),
    ));
    out.push(check(
        "callbacks delivered",
        n("callbacks_sent").is_some_and(|s| s >= 1) && n("callbacks_failed") == Some(0),
        format!(
            "sent={:?} failed={:?}",
            n("callbacks_sent"),
            n("callbacks_failed")
        ),
    ));
    out.extend(pixel_checks(app));
    let (acquired, released) = (n("buffers_acquired"), n("buffers_released"));
    out.push(check(
        "buffers released",
        acquired.is_some_and(|a| a > FRAMES) && acquired == released,
        format!("acquired={acquired:?} released={released:?}: the warm-up's frame and the {FRAMES} of the run, each released once (a BBQ that is not released stalls)"),
    ));
    let (on_channel, peer_gone) = (n("releases_on_channel"), n("releases_peer_gone"));
    out.push(check(
        "release channel",
        on_channel.is_some_and(|c| c > 0)
            && on_channel.zip(peer_gone).map(|(c, p)| c + p) == released,
        format!("on the channel={on_channel:?} to a queue already gone={peer_gone:?}, together all {released:?} releases"),
    ));
    let lock = client_value(app, "max_lock_ms");
    out.push(check(
        "no stall",
        client_says(app, &format!("frames posted={FRAMES} "))
            && lock.is_some_and(|ms| ms < MAX_LOCK_MS),
        format!("{FRAMES} frames posted, max lockCanvas {lock:?} ms (limit {MAX_LOCK_MS})"),
    ));
    let (baseline, after) = (client_value(app, "baseline"), client_value(app, "after"));
    out.push(check(
        "fds",
        n("fds_held") == Some(0)
            && n("fds_taken").is_some_and(|t| t > 0)
            && n("fds_taken") == n("fds_closed")
            && baseline.is_some()
            && baseline == after,
        format!(
            "compositor holds {:?}, took {:?} closed {:?}; the client's open descriptors {baseline:?} before, {after:?} after",
            n("fds_held"),
            n("fds_taken"),
            n("fds_closed")
        ),
    ));
    // the interval of a refresh rate, in microseconds
    let interval_us = 1e6 / f64::from(display.refresh_hz);
    for (label, key, tolerance) in [
        (
            "frame cadence",
            "mean_frame_interval_us",
            FRAME_INTERVAL_TOLERANCE,
        ),
        (
            "wall cadence",
            "mean_wall_interval_us",
            WALL_INTERVAL_TOLERANCE,
        ),
    ] {
        let got = client_value(app, key);
        let ok = got.is_some_and(|g| ((g as f64) - interval_us).abs() <= interval_us * tolerance);
        out.push(check(
            label,
            ok,
            format!(
                "{key}={got:?}, display {interval_us:.0} us +-{:.0} %",
                tolerance * 100.0
            ),
        ));
    }
    finish(out, app)
}

/// The checks that read the whole log.
fn finish(mut out: Vec<Check>, app: &str) -> Vec<Check> {
    for (label, text) in [
        ("no crash", "FATAL EXCEPTION"),
        ("no unserved-service wait", "Waited one second for"),
        // Rust's message when a thread panics; one inside an extern "C" callback aborts the process ("panic in a function that cannot unwind"), once seen at exit
        ("no panic", "panicked at"),
    ] {
        let hit = app.lines().find(|l| l.contains(text));
        out.push(check(
            label,
            hit.is_none(),
            hit.unwrap_or("none").to_owned(),
        ));
    }
    out
}

/// One `composer: quad` line of the compositor's report, as the sink writes it (sink.rs `ReportSink`).
fn quad_line(scene: u32, name: &str, digest: u64) -> String {
    format!(
        "composer: quad frame={scene} layer=2 name={name} buffer={SIZE}x{SIZE} frame_number={scene} dst=1,2,65,66 digest={digest:016x}\n"
    )
}

/// A client log of a good run: the warm-up's frame, then frames 1 to 30 each shown (some twice, as a moved layer is presented again), every step, the counters of such a run, a cadence of exactly 60 Hz.
fn good_log() -> String {
    log_showing(&(1..=FRAMES as u32).collect::<Vec<_>>())
}

/// The good log with the main layer showing `frames` in that order.
fn log_showing(frames: &[u32]) -> String {
    let mut quads = quad_line(1, WARM_LAYER, expected_digest(0));
    for &n in frames {
        quads += &quad_line(n + 1, MAIN_LAYER, expected_digest(n));
        if n % 5 == 0 {
            quads += &quad_line(n + 1, MAIN_LAYER, expected_digest(n));
        }
    }
    format!(
        "{}\n\
         1 1 1 I liblog/AtlComposer: start\n\
         1 1 1 I liblog/AtlComposer: fds baseline=41\n\
         2 1 1 I liblog/AtlComposer: layers created valid=true\n\
         2 1 1 I liblog/AtlComposer: bbq created surface valid=true\n\
         3 1 1 I liblog/AtlComposer: vsync frames=30 complete=true\n\
         3 1 1 I liblog/AtlComposer: frames posted=30 max_lock_ms=3\n\
         4 1 1 I liblog/AtlComposer: vsync mean_frame_interval_us=16666 mean_wall_interval_us=16700\n\
         5 1 1 I liblog/AtlComposer: committed callback fired=true\n\
         {quads}\
         composer: counters ticks=50 transactions=66 layers_created=4 layers_alive=0 transactions_applied=66 layer_states=95 unknown_layer=0 buffers_acquired=31 buffers_released=31 completions=1 scenes=36 vsync_sent=50 vsync_dropped=0 callbacks_sent=1 callbacks_failed=0 releases_on_channel=29 releases_peer_gone=2 fds_held=0 fds_taken=62 fds_closed=62 ignored=\n\
         5 1 1 I liblog/AtlComposer: fds after=41\n\
         6 1 1 I liblog/AtlComposer: done\n",
        served_line()
    )
}

fn good_services() -> String {
    format!("{}\n", agent_line())
}

/// atl-composer start's line once both services are in the process's service view.
fn served_line() -> String {
    use atl_services_types::compositor_registry::{SURFACE_FLINGER, SURFACE_FLINGER_AIDL};
    format!("composer: serving {SURFACE_FLINGER}, {SURFACE_FLINGER_AIDL} in-process")
}

/// The registry's line once the client's UI agent and compositor are registered (registry_service.rs).
fn agent_line() -> String {
    format!("services: registered the UI agent and compositor of uid {APP_UID}")
}

/// One synthetic run and the checks that must fail on it (empty: the run must pass).
struct SelfCase {
    name: &'static str,
    app: String,
    services: String,
    failing: &'static [&'static str],
}

fn self_cases() -> Vec<SelfCase> {
    let good = good_log();
    let case = |name, app: String, services: String, failing| SelfCase {
        name,
        app,
        services,
        failing,
    };
    vec![
        case("good run", good.clone(), good_services(), &[]),
        case(
            "not registered",
            good.clone(),
            String::new(),
            &["registered"],
        ),
        case(
            "a transaction not applied",
            good.replace("transactions_applied=66", "transactions_applied=65"),
            good_services(),
            &["transactions applied"],
        ),
        case(
            "too few transactions",
            good.replace("transactions=66 ", "transactions=29 ")
                .replace("transactions_applied=66", "transactions_applied=29"),
            good_services(),
            &["transactions applied"],
        ),
        case(
            "a layer outlives its handle",
            good.replace("layers_alive=0", "layers_alive=1"),
            good_services(),
            &["layers"],
        ),
        case(
            "vsync dropped",
            good.replace("vsync_dropped=0", "vsync_dropped=2"),
            good_services(),
            &["vsync delivered"],
        ),
        case(
            "callback failed",
            good.replace("callbacks_failed=0", "callbacks_failed=1"),
            good_services(),
            &["callbacks delivered"],
        ),
        case(
            "wrong frame cadence",
            good.replace(
                "mean_frame_interval_us=16666",
                "mean_frame_interval_us=33333",
            ),
            good_services(),
            &["frame cadence"],
        ),
        case(
            "wrong wall cadence",
            good.replace("mean_wall_interval_us=16700", "mean_wall_interval_us=40000"),
            good_services(),
            &["wall cadence"],
        ),
        case(
            "a crash",
            good.clone() + "7 1 1 E AndroidRuntime: FATAL EXCEPTION: main\n",
            good_services(),
            &["no crash"],
        ),
        case(
            "an unserved-service wait",
            good + "7 1 1 W libbinder: Waited one second for SurfaceFlingerAIDL\n",
            good_services(),
            &["no unserved-service wait"],
        ),
        case(
            "a panic at exit",
            good_log()
                + "thread '<unnamed>' (12) panicked at library/std/src/thread/local.rs:429:25:\n",
            good_services(),
            &["no panic"],
        ),
        case(
            "no counters line",
            "AtlComposer: done\n".to_owned(),
            good_services(),
            &["compositor counters"],
        ),
        case(
            "no bbq surface",
            good_log().replace("bbq created surface valid=true", "bbq failed: x"),
            good_services(),
            &["bbq surface"],
        ),
        case(
            "a shown buffer is not any frame's picture",
            good_log().replacen(
                &format!("{:016x}", expected_digest(5)),
                &format!("{:016x}", expected_digest(31)),
                1,
            ),
            good_services(),
            &["pixels"],
        ),
        case(
            "only ten frames shown",
            log_showing(&(1..=10).collect::<Vec<_>>()),
            good_services(),
            &["pixels"],
        ),
        case(
            "frames shown out of order",
            log_showing(
                &(1..=FRAMES as u32)
                    .map(|n| match n {
                        10 => 11,
                        11 => 10,
                        other => other,
                    })
                    .collect::<Vec<_>>(),
            ),
            good_services(),
            &["pixels"],
        ),
        case(
            "a buffer the sink could not read",
            good_log().replacen("digest=", "unreadable=", 1),
            good_services(),
            &["quads readable", "pixels"],
        ),
        case(
            "a buffer never released",
            good_log().replace("buffers_released=31", "buffers_released=30"),
            good_services(),
            &["buffers released", "release channel"],
        ),
        case(
            "no release went down the channel",
            good_log().replace(
                "releases_on_channel=29 releases_peer_gone=2",
                "releases_on_channel=0 releases_peer_gone=31",
            ),
            good_services(),
            &["release channel"],
        ),
        case(
            "the compositor still holds a descriptor",
            good_log().replace("fds_held=0", "fds_held=1"),
            good_services(),
            &["fds"],
        ),
        case(
            "the client's descriptor count drifts",
            good_log().replace("fds after=41", "fds after=43"),
            good_services(),
            &["fds"],
        ),
        case(
            "a descriptor taken and never closed",
            good_log().replace("fds_closed=62", "fds_closed=61"),
            good_services(),
            &["fds"],
        ),
        case(
            "lockCanvas stalls",
            good_log().replace("max_lock_ms=3", "max_lock_ms=3000"),
            good_services(),
            &["no stall"],
        ),
        case(
            "no frame posted",
            good_log().replace("frames posted=30", "frames posted=0"),
            good_services(),
            &["no stall"],
        ),
    ]
}

/// `--self-test`: each case fails exactly the checks it names.
fn self_test() -> ExitCode {
    let display = DisplayModel::default();
    let mut all = true;
    let mut failures: Vec<Failure> = Vec::new();
    for c in self_cases() {
        let failed: Vec<&str> = evaluate(&c.app, &c.services, &display)
            .into_iter()
            .filter(|k| !k.ok)
            .map(|k| k.name)
            .collect();
        // a run without counters stops early: only its own check is reported
        let holds = if c.failing == ["compositor counters"] {
            failed.contains(&"compositor counters")
        } else {
            failed == c.failing
        };
        all &= holds;
        println!(
            "composer: self-test {}: {} (failed: {failed:?})",
            c.name,
            if holds { "pass" } else { "FAIL" }
        );
        if !holds {
            failures.push(Failure::step(
                c.name,
                format!("failed {failed:?}, the case expects {:?}", c.failing),
            ));
        }
    }
    if all {
        ExitCode::SUCCESS
    } else {
        let verdict = Verdict {
            cmd: "composer",
            total: None,
            skipped: Vec::new(),
            failures,
        };
        println!("{}", verdict.line());
        ExitCode::FAILURE
    }
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.self_test {
        return Ok(self_test());
    }
    let _ = Layout::of(root)?;
    // atld answers with the monitor's refresh rate; the frame cadence is judged against it
    let display = run_display();
    let rt = Runtime::new(
        root,
        "composer",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "composer-art",
        args.verbose,
    )?
    .with_gralloc(root)?;
    // the display this gate judges against: atld takes it as given and never asks the desktop
    let rt = rt.with_display(display.to_arg());
    let rt = if args.break_view {
        rt.with_ui_break("no-serve")
    } else {
        rt
    };
    let work = rt.out.join("composer");
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("apk"), &[], None, None)?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    println!(
        "composer: built {} ({} versionCode {})",
        apk.display(),
        m.name,
        m.version_code
    );
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: None,
        packages: vec![Package::base(apk.clone())],
        first_uid: APP_UID,
        libraries: vec![],
        shell: Some(Shell {
            class: CLIENT_CLASS.to_owned(),
            uid: APP_UID,
            name: CLIENT_NAME.to_owned(),
            args: vec![],
            boot_jars: vec![apk],
        }),
        log_tags: args.log_tags.clone(),
    };
    let started = Instant::now();
    let mut atld_child = rt.start_atld(&run)?;
    let status = loop {
        if let Some(s) = atld_child.try_wait()? {
            break Some(s);
        }
        if started.elapsed() > LIMIT {
            let _ = atld_child.kill();
            atld_child.wait()?;
            break None;
        }
        std::thread::sleep(POLL);
    };
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let app = read(&run.log(CLIENT_NAME));
    let services = read(&run.log("services"));
    if args.verbose {
        println!("---- client log\n{app}");
    }
    let mut ok = true;
    let mut failures: Vec<Failure> = Vec::new();
    let exited = status.is_some_and(|s| s.success());
    let atld = match status {
        Some(s) => format!("exited {s} (the client's status)"),
        None => format!("did not exit within {LIMIT:?}"),
    };
    println!(
        "composer: {} atld {atld}",
        if exited { "pass" } else { "FAIL" }
    );
    if !exited {
        failures.push(Failure::step("atld", atld));
    }
    ok &= exited;
    for c in evaluate(&app, &services, &display) {
        println!(
            "composer: {} {}: {}",
            if c.ok { "pass" } else { "FAIL" },
            c.name,
            c.detail
        );
        if !c.ok {
            failures.push(Failure::step(c.name, c.detail.clone()));
        }
        ok &= c.ok;
    }
    if let Some(l) = last_counters(&app) {
        println!("{l}");
    }
    for l in app.lines().filter(|l| l.contains("diag: ")) {
        println!("composer: counted {l}");
    }
    for l in counter_lines(&services) {
        println!("composer: services counted {l}");
    }
    if !ok {
        println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
        println!("---- client log (last {TAIL} lines)\n{}", tail(&app, TAIL));
        println!(
            "---- services log (last {TAIL} lines)\n{}",
            tail(&services, TAIL)
        );
        let verdict = Verdict {
            cmd: "composer",
            total: None,
            skipped: Vec::new(),
            failures,
        };
        println!("{}", verdict.line());
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every self-test case fails exactly the checks it names.
    #[test]
    fn self_cases_hold() {
        let display = DisplayModel::default();
        for c in self_cases() {
            let failed: Vec<&str> = evaluate(&c.app, &c.services, &display)
                .into_iter()
                .filter(|k| !k.ok)
                .map(|k| k.name)
                .collect();
            if c.failing == ["compositor counters"] {
                assert!(failed.contains(&"compositor counters"), "{}", c.name);
            } else {
                assert_eq!(failed, c.failing, "{}", c.name);
            }
        }
    }

    /// The published FNV-1a 64 test vectors, as atl-composer's sink checks its own.
    #[test]
    fn fnv1a64_known_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// Frame 0 by hand from the formula: (r, g, b) = (0, 17, 255), so the quadrants are (0, 17, 255), (17, 255, 0), (255, 0, 17) and (255 - 0, 255 - 17, 255 - 255) = (255, 238, 0); the corners and the quadrant boundaries.
    #[test]
    fn expected_pixels_follow_the_formula() {
        let p = expected_pixels(0);
        assert_eq!(p.len(), 64 * 64 * 4);
        let at = |x: usize, y: usize| &p[(y * 64 + x) * 4..][..4];
        assert_eq!(at(0, 0), [0, 17, 255, 255]);
        assert_eq!(at(31, 31), [0, 17, 255, 255]);
        assert_eq!(at(32, 0), [17, 255, 0, 255]);
        assert_eq!(at(0, 32), [255, 0, 17, 255]);
        assert_eq!(at(63, 63), [255, 238, 0, 255]);
        // frame 30: r = 37 * 30 mod 256 = 1110 - 1024 = 86, g = (91 * 30 + 17) mod 256 = 2747 - 2560 = 187, b = (255 - 150) mod 256 = 105
        assert_eq!(&expected_pixels(30)[..4], [86, 187, 105, 255]);
    }

    /// The counters line's fields are read by exact key: `transactions` is not `transactions_applied`.
    #[test]
    fn field_matches_whole_keys() {
        let line = "composer: counters transactions=30 transactions_applied=29";
        assert_eq!(field(line, "transactions"), Some(30));
        assert_eq!(field(line, "transactions_applied"), Some(29));
        assert_eq!(field(line, "missing"), None);
    }
}
