// SPDX-License-Identifier: MIT
//! `xtask run-apk <ROW|PATH>`: a test APK row of test-apks.toml, or an APK file given by path (unpinned), started by atld the way an app is (its launcher activity), in UI mode: the app's window is a GTK window of this desktop with --hold, else of the gate's headless compositor (nested_wayland.rs).
//! Staging is the window gate's: the runtime (Runtime::new, gralloc, the display), the APK copied into the run's work dir, the package file from the APK's real manifest (manifest::read).
//! The data root is atld's default, the user's in the XDG data dir (`--data-root` another): an app's data survives launches and build dirs, as on a device, until `--clear-data` (`pm clear` of the package) or the dir is removed.
//! No probe; injected input only with `--pointer` (a scripted tap sequence the app runs from its first present). The verdict is about the last activity the app resumed (`services: activity resumed <class>`, sent after onResume returns). The run waits until the services
//! are settled (`services: activity settled`: no launch, start or lifecycle step unacknowledged), so a Splash that starts Main is judged on Main. Pass: that resume is the last one, a frame is
//! presented after it (`atl-composer: ui: present`), and no fault in the app log within the watch after it (window::fault_after). The run ends once all of that holds and the watch is over
//! (--hold: the hold); a run that ends before its watch is over fails.
//! `--drive <script>`: once that wait is over, the script runs against the window (drive.rs for its grammar); a failed step fails the run, and the verdict covers the run up to the script's end.
//! `remote run-apk` ([`remote`]): the same run on a target's desktop. The bundle goes there and is checked, atld runs there for `--hold`, and the logs come back when the run ends; the verdict is read from them, against the target's end record.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use crate::atld_run::{
    MS_PER_S, Package, PackageFile, Run, Runtime, last_resumed, parse_stamp, run_display, settled,
};
use crate::manifest::Manifest;
use atl_services_types::native_libs::{LIB_DIR_NAME, SupportedAbi};

use crate::nested_wayland::NestedCompositor;
use crate::{drive, manifest, observe, remote, test_apks, verdict, window};
use xtask::timing;

/// The app process's log line for a presented frame (atl-composer ui_window.rs, debug level: UI mode enables it).
pub(crate) const PRESENT_LINE: &str = "atl-composer: ui: present";
/// The staged runtime's name, shared with the window gate: one ART install for both.
pub(crate) const STAGE: &str = "window-art";
/// What a remote run returns under its scratch dir besides the logs: atld's log, its exit code (`atld.rc`) and its end time (`end.ms`, the target's clock).
const END_FILES: [&str; 3] = ["atld.log", "atld.rc", "end.ms"];
/// How long past the target's limit the transfer may run (its tar and ssh) before it is stopped and the run fails.
pub(crate) const TRANSFER_MARGIN: Duration = Duration::from_secs(60);

#[derive(clap::Args)]
pub struct Args {
    /// A row of test-apks.toml (lowercase letters, digits, `-`): its pinned APK is downloaded and verified into `<workspace>/cache/test-apks` on first use.
    /// Any other argument is a file path (it holds `.`, `/` or `\`), relative to the current directory: that APK runs unpinned, no row checks it.
    #[arg(value_name = "ROW|PATH")]
    target: String,
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Keeps the app up this many seconds after its resume, the window open for drags: no probe, no injected input. Without it the run ends once a frame is presented and the watch is over.
    #[arg(long, value_name = "SECONDS")]
    hold: Option<u64>,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
    /// Deliberate-failure check: apps run without the app seccomp filter (atl-broker seccomp.rs). Without the filter the app's queued-work looper dies with SecurityException.
    #[arg(long)]
    no_app_seccomp: bool,
    /// Deliberate-failure check: every device ABI maps to the host ISA (atld_run `with_broken_instruction_set`), so a foreign-ABI app gets the host ISA from the broker and its bridge runs identity.
    #[arg(long)]
    break_isa: bool,
    /// Deliberate-failure check of the render-path choice: atld `--break-render-path` probes each candidate without its nodes, so a host that offers one must fail `render path` (offered, not taken).
    #[arg(long)]
    break_render_path: bool,
    /// Deliberate-failure check of FEATURE_WEBVIEW: the run's library dir lacks the web engine module (atld_run `without_webview_engine`) = a host without WebKit, so atld declares no
    /// FEATURE_WEBVIEW and the app gets AOSP's no-WebView device.
    #[arg(long)]
    break_webview_engine: bool,
    /// A pointer script the app injects into its window from its first present (atl-composer `input::parse_script`): `down:X,Y up:X,Y` in the content's physical pixels, `wait:MS` between steps,
    /// e.g. `wait:6000 down:792,28 up:792,28` taps (792, 28) 6 s after the first frame. The verdict is still about the last resumed activity, so wait for each tap's answer to settle.
    #[arg(long, value_name = "SCRIPT", conflicts_with = "drive")]
    pointer: Option<String>,
    /// A script file run against the window on the headless compositor, one step a line, coordinates in window content pixels: `wait <ms>`, `wait-frames <n>`, `tap <x> <y>`, `swipe <x1> <y1> <x2> <y2> <ms>`, `key <name>` (a letter, a digit, or escape, backspace, tab, enter, space, home, end, up, down, left, right, pageup, pagedown, back), `resize <w> <h>`, `shot <file.png>` (the window's content; a one-colour capture fails the run).
    /// The app is stopped after the last step; a failed step exits non-zero.
    #[arg(long, value_name = "FILE", conflicts_with = "hold")]
    drive: Option<PathBuf>,
    /// Deliberate-failure check of --drive: every shot is blanked, so the first one must fail the run.
    #[arg(long, requires = "drive")]
    break_blank_capture: bool,
    /// atld `--tilt-keys`: held left / right (or a / d) tilt the device for accelerometer games, at atld's default angle, ramp and rate.
    #[arg(long)]
    tilt_keys: bool,
    /// atld `--mouse-touch`: the left mouse button (a drive's `tap` / `swipe`) reaches the app as one finger. atld's default, on; `false` is exact AOSP input.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    mouse_touch: bool,
    /// atld `--esc-back`: an Esc the app leaves unhandled falls back to Back. atld's default, on; `false` is AOSP's key map.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    esc_back: bool,
    /// atld `--exit-with-last-app`: the run ends once the app ended after its window was closed, or it finished its last activity or exited 0 (the verdict names which). atld's UI default,
    /// on; `false` keeps atld until the hold ends.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    exit_with_last_app: bool,
    /// A variable in every app process's environment (repeat), atld `--app-env`: test-environment setup such as `LD_DEBUG=bindings` (the launcher-closure measurement; glibc's lines land in the app log). The product reads none of these.
    #[arg(long = "app-env", value_name = "KEY=VALUE")]
    app_env: Vec<String>,
    /// `pm clear` of the package before the launch: its data (user and device-protected user dirs, its external dirs) left by earlier launches is removed, so the app starts from empty data.
    /// Other packages' data and the system state stay. The data root is kept across launches: without this flag the app keeps its data.
    #[arg(long)]
    clear_data: bool,
    /// The data root (atld `--data-root`): `apps/<package>/` and `device/`, kept across launches. Default: atld's, the user's in the XDG data dir (`$XDG_DATA_HOME/atl`, else `~/.local/share/atl`), shared by every checkout and build dir.
    /// A fresh device is a new or empty DIR.
    #[arg(long, value_name = "DIR")]
    data_root: Option<PathBuf>,
}

/// `xtask remote run-apk` (remote.rs `Remote::RunApk`): the same run on a target's desktop. `--hold` is required: the window is the target's, on its Wayland session, not a headless one.
#[derive(clap::Args)]
pub struct RemoteArgs {
    /// A row of test-apks.toml, or an APK file path, as for `run-apk`.
    #[arg(value_name = "ROW|PATH")]
    target: String,
    /// The target's arch, as for `remote test app`: the run's out dir is the build dir's (a cross host's under it) unless `--out`.
    #[command(flatten)]
    target_arch: crate::remote::TargetArch,
    /// ssh alias from WSL `~/.ssh/config` (its `Host` name), or `local:<absolute dir>` (a directory standing in for the target).
    #[arg(long)]
    host: String,
    /// Keeps the app up this many seconds after its resume, the window open on the target's desktop.
    #[arg(long, value_name = "SECONDS")]
    hold: u64,
    /// The display atld answers with (`WxH@hz,dpi`), instead of the target's monitor. Default: the target's own monitor, read by atld on the target's desktop.
    #[arg(long, value_name = "DISPLAY")]
    display: Option<String>,
    /// atld `--tilt-keys`, as for `run-apk`.
    #[arg(long)]
    tilt_keys: bool,
    /// atld `--mouse-touch`, as for `run-apk`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    mouse_touch: bool,
    /// atld `--esc-back`, as for `run-apk`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    esc_back: bool,
    /// atld `--exit-with-last-app`, as for `run-apk`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    exit_with_last_app: bool,
    /// A pointer script atld injects from the app's first present (`run-apk --pointer`).
    #[arg(long, value_name = "SCRIPT")]
    pointer: Option<String>,
    /// A variable in atld's environment on the target, `KEY=VALUE`, repeatable: what the desktop sets for a launch, e.g. NVIDIA's offload `__NV_PRIME_RENDER_OFFLOAD=1` (the run is otherwise `env -i`).
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env)]
    env: Vec<(String, String)>,
    /// The native out dir holding the record, the bundle and the run (default: this checkout's build dir, see `cargo xtask out show`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Pushes the bundle, runs the check there, then the app. Without it: prints the plan.
    #[arg(long)]
    apply: bool,
    /// Deliberate break (`remote push`'s values): the bundle without the named file, which the check then names.
    #[arg(long = "break", value_enum)]
    brk: Option<remote::Break>,
    /// Records the target's Hyprland session while the app holds: one record a second in `observe.jsonl` (its `atl.*` windows), and a verdict line. A target without exactly one Hyprland session is skipped, the reason logged.
    #[arg(long)]
    observe: bool,
    /// Shots of the app's window, ms after its first present, comma-separated (`5000,20000`): `grim -g` of the window's rect only, never the desktop. Implies `--observe`.
    #[arg(long, value_name = "MS", value_delimiter = ',')]
    shot: Vec<u64>,
}

impl Args {
    /// A `--drive` run of the APK file `apk`, for `xtask test drive`: its steps go to [`run_on`], not through `drive`.
    pub(crate) fn driven(
        apk: &Path,
        host: &str,
        out: Option<PathBuf>,
        java_out: Option<PathBuf>,
        break_blank_capture: bool,
    ) -> Self {
        Self {
            target: apk.display().to_string(),
            host: host.to_owned(),
            out,
            java_out,
            hold: None,
            log_tags: "*:i".to_owned(),
            verbose: false,
            no_app_seccomp: false,
            break_isa: false,
            break_render_path: false,
            break_webview_engine: false,
            pointer: None,
            drive: None,
            break_blank_capture,
            tilt_keys: false,
            mouse_touch: false,
            esc_back: false,
            exit_with_last_app: false,
            app_env: Vec::new(),
            clear_data: false,
            data_root: None,
        }
    }

    /// The same run with `--clear-data` set to `clear` (the gates that launch an app twice and check its data).
    pub(crate) fn with_clear_data(mut self, clear: bool) -> Self {
        self.clear_data = clear;
        self
    }

    /// The same run on the data root `dir` (`--data-root`): a gate's own, never the user's.
    pub(crate) fn with_data_root(mut self, dir: &Path) -> Self {
        self.data_root = Some(dir.to_path_buf());
        self
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let (pin, files) = source(root, &args.target)?;
    // a bad script fails before anything starts
    let steps = match &args.drive {
        Some(file) => {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?;
            Some(drive::parse(&text).with_context(|| file.display().to_string())?)
        }
        None => None,
    };
    let (rt, foreign) = runtime(root, args, &files)?;
    // the data root outlives the launch and the build dir: the app's data is still there at the next one (`--clear-data` clears one package of it)
    let data_root = match &args.data_root {
        Some(d) => d.clone(),
        None => {
            let user = atl_platform_host::user_data_root()
                .context("no default data root: neither XDG_DATA_HOME nor HOME names an absolute dir; pass --data-root")?;
            // into the old single tree's place: atld then moves its packages into apps/<package>/ (crates/atld host_layout.rs)
            migrate_build_dir_device(&legacy_device_dir(&rt.out), &user.join("device"))?;
            user
        }
    };
    println!("run-apk: data root {}", data_root.display());
    // the desktop's fonts, as a user's run sees them (the pinned test set has no CJK or emoji face)
    let rt = rt.with_data_root(&data_root).with_desktop_fonts();
    run_on(&rt, args, foreign.as_ref(), pin.as_ref(), files, steps)
}

/// Where run-apk kept the device before it moved to the user's data dir (b7c61763): under the build dir `out`, so a pruned or other build dir lost it.
fn legacy_device_dir(out: &Path) -> PathBuf {
    out.join("run-apk").join("device")
}

/// One-time move of the build dir's device `old` to the user's device `new`: only when `old` exists and `new` does not. Both present: `old` is left, named, never merged or deleted.
/// Across filesystems (build pool vs home): copied to `<new>.migrating`, renamed into place, then `old` removed, so an interrupted copy never shows as the device.
/// Delete this (and [`legacy_device_dir`]) once no build dir from before b7c61763's successor holds a `run-apk/device`.
fn migrate_build_dir_device(old: &Path, new: &Path) -> Result<()> {
    if !old.is_dir() {
        return Ok(());
    }
    if new.exists() {
        println!(
            "run-apk: old device {} left in place: {} exists (not merged)",
            old.display(),
            new.display()
        );
        return Ok(());
    }
    let parent = new.parent().context("device dir has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    match std::fs::rename(old, new) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            let mut tmp = new.as_os_str().to_os_string();
            tmp.push(".migrating");
            let tmp = PathBuf::from(tmp);
            if tmp.exists() {
                std::fs::remove_dir_all(&tmp)
                    .with_context(|| format!("removing {}", tmp.display()))?;
            }
            let ok = std::process::Command::new("cp")
                .arg("-a")
                .arg("--reflink=auto")
                .arg(old)
                .arg(&tmp)
                .status()
                .context("running cp")?
                .success();
            ensure!(
                ok,
                "copying {} to {}: cp failed, old device kept",
                old.display(),
                tmp.display()
            );
            std::fs::rename(&tmp, new).with_context(|| format!("renaming {}", tmp.display()))?;
            std::fs::remove_dir_all(old).with_context(|| format!("removing {}", old.display()))?;
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("moving {} to {}", old.display(), new.display()));
        }
    }
    println!(
        "run-apk: moved device {} -> {}",
        old.display(),
        new.display()
    );
    Ok(())
}

/// One run of `args` on the staged runtime `rt` ([`configure`]d for `foreign`) of the package `files` (`pin`: its row). A drive gate's runs all share `rt`: each run's wiring goes on a copy, so none carries over to the next.
pub(crate) fn run_on(
    rt: &Runtime,
    args: &Args,
    foreign: Option<&SupportedAbi>,
    pin: Option<&test_apks::Row>,
    files: Vec<PackageFile>,
    steps: Option<Vec<drive::Step>>,
) -> Result<ExitCode> {
    let work = rt.out.join("run-apk");
    std::fs::create_dir_all(&work)?;
    // without --hold nobody watches the window: it maps in the gate's own compositor, which outlives atld (declared before it)
    let (nested, proxy) = timing::phase("run: compositor", || -> Result<_> {
        let nested = match (args.hold, &steps) {
            (Some(_), _) => None,
            (None, Some(_)) => Some(NestedCompositor::start_placed(&work.join("nested"))?),
            (None, None) => Some(NestedCompositor::start(&work.join("nested"))?),
        };
        // a script's input goes through the proxy
        let proxy = match (&nested, &steps) {
            (Some(n), Some(_)) => Some(window::start_proxy(&n.socket)?),
            _ => None,
        };
        Ok((nested, proxy))
    })?;
    // the display at the rate of the compositor the window maps on; on a copy, so the shared runtime keeps no run's wiring
    let rt = match (&nested, &proxy) {
        (Some(n), Some(p)) => rt
            .clone()
            .with_wayland(&p.listen)
            .with_display(n.display().to_arg()),
        (Some(n), None) => rt
            .clone()
            .with_wayland(&n.socket)
            .with_display(n.display().to_arg()),
        (None, _) => rt.clone().with_display(run_display().to_arg()),
    };
    let m = timing::phase("run: package", || package(&rt, &files, pin, &args.target))?;
    // the name becomes the log and data paths below: refuse a name that is no package name before any is built
    ensure!(
        atl_android_paths::valid_package_name(&m.name),
        "run-apk: the manifest's package {:?} is no package name: nothing started",
        m.name
    );
    if args.clear_data {
        let data_root = rt
            .data_root()
            .context("--clear-data: no persistent data root (run() sets one)")?;
        crate::atld_run::clear_package_data(data_root, &m.name)?;
        println!("run-apk: --clear-data: {} starts from empty data", m.name);
    }
    let run = run_of(&work, files, &m, &args.log_tags);
    let app_log = run.log(&m.name);
    let services_log = run.log("services");
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let hold_ms = args.hold.map(|s| s * MS_PER_S);
    let watch_ms = hold_ms.unwrap_or(window::WINDOW_MS);
    // a pointer script runs from the first present: the run lasts at least its waits plus the watch, and faults count from the last resume to the run's end
    let script_ms = args.pointer.as_deref().map_or(0, script_wait_ms);
    let started = Instant::now();
    let mut atld = timing::phase("run: atld start", || rt.start_atld(&run))?;
    let booted = Instant::now();
    let mut resumed_at = None;
    let mut presented_at = None;
    let mut done_at = None;
    let mut ended = None;
    let mut complete = false;
    let mut driven = None;
    loop {
        let services = read(&services_log);
        if resumed_at.is_none() && last_resumed(&services).is_some() {
            resumed_at = Some(Instant::now());
        }
        if presented_at.is_none() && presented_since(&read(&app_log), 0) > 0 {
            presented_at = Some(Instant::now());
        }
        let scripted = started.elapsed() >= Duration::from_millis(script_ms + watch_ms);
        let done = scripted
            && last_resumed(&services).is_some_and(|r| {
                window::now_ms() >= r.ms + watch_ms
                    && settled(&services)
                    && (hold_ms.is_some() || presented_since(&read(&app_log), r.ms) > 0)
            });
        if done {
            complete = true;
            done_at = Some(Instant::now());
            if let (Some(steps), Some(proxy), Some(nested)) = (&steps, &proxy, &nested) {
                let target = drive::Target {
                    proxy,
                    nested,
                    app_log: &app_log,
                    present_line: PRESENT_LINE,
                    alive: &|| !run.app_ended(),
                    break_blank: args.break_blank_capture,
                };
                driven = Some(timing::phase("run: script", || drive::run(steps, target)));
            }
            break;
        }
        if let Some(s) = atld.try_wait()? {
            ended = Some(s);
            break;
        }
        // the app's end is the run's end here, as when atld ended with it
        if run.app_ended() {
            ended = Some(run.stop(&mut atld)?);
            break;
        }
        if started.elapsed() > window::LIMIT + Duration::from_secs(args.hold.unwrap_or(0)) {
            break;
        }
        std::thread::sleep(window::POLL);
    }
    // boot: start_atld's return to the app's resume, then to its first present; watch: the first present to the script's start, or to the loop's end
    let loop_end = Instant::now();
    let resumed = resumed_at.unwrap_or(presented_at.unwrap_or(loop_end));
    timing::add(
        "run: boot to activity resumed",
        resumed.saturating_duration_since(booted),
    );
    if let Some(from) = resumed_at {
        timing::add(
            "run: resumed to first present",
            presented_at
                .unwrap_or(loop_end)
                .saturating_duration_since(from),
        );
    }
    if let Some(from) = presented_at {
        timing::add(
            "run: watch",
            done_at.unwrap_or(loop_end).saturating_duration_since(from),
        );
    }
    if ended.is_none() {
        timing::phase("run: stop", || -> Result<()> {
            window::stop_apps(atld.id());
            run.stop(&mut atld)?;
            Ok(())
        })?;
    }
    let app = read(&app_log);
    let services = read(&services_log);
    if args.verbose {
        println!("---- app log\n{app}");
    }
    let ending = Ending {
        complete,
        atld_exited: ended.is_some(),
        end_ms: window::now_ms(),
        app_end: app_end(&services, &read(&run.atld_log())),
    };
    let scripted = args.pointer.is_some() || steps.is_some();
    let outcome = check_delivery(foreign, &m.name, &app)
        .and_then(|()| judge_ending(&services, &app, &ending, watch_ms, scripted))
        .and_then(|ok| {
            render_path_check(&read(&run.atld_log()), &app).map(|p| format!("{ok}; {p}"))
        });
    // the drive's failure first: it says what the script could not do, the verdict only what the app did
    let outcome = match (driven, steps.is_some()) {
        (Some(Err(e)), _) => Err(format!("drive: {e:#}")),
        (None, true) => outcome.and(Err("the script never ran".to_owned())),
        _ => outcome,
    };
    Ok(finish(
        outcome,
        &app,
        &services,
        &run.atld_log(),
        &run.root.join("log"),
    ))
}

/// `xtask remote run-apk` (see [`RemoteArgs`]): the run on a target's desktop. Dry run by default: prints the bundle and the plan. `--apply` records the run, pushes the bundle, runs the target's check, then runs atld there with its limit at `window::LIMIT` + the hold (+ a pointer script's waits). The logs come back when the run ends, and the verdict is `run-apk`'s, on the target's clock.
pub fn remote(root: &Path, a: &RemoteArgs) -> Result<ExitCode> {
    let mut shots = a.shot.clone();
    shots.sort_unstable();
    shots.dedup();
    ensure!(
        shots.iter().all(|&ms| ms > 0 && ms <= a.hold * MS_PER_S),
        "--shot: each time is ms after the app's first present and within the hold ({} s), got {:?}",
        a.hold,
        a.shot
    );
    let observe_on = a.observe || !shots.is_empty();
    let (pin, files) = source(root, &a.target)?;
    let args = Args {
        target: a.target.clone(),
        host: a.target_arch.resolve(root)?,
        out: a.out.clone(),
        java_out: None,
        hold: Some(a.hold),
        log_tags: "*:i".to_owned(),
        verbose: false,
        no_app_seccomp: false,
        break_isa: false,
        break_render_path: false,
        break_webview_engine: false,
        pointer: a.pointer.clone(),
        drive: None,
        break_blank_capture: false,
        tilt_keys: a.tilt_keys,
        mouse_touch: a.mouse_touch,
        esc_back: a.esc_back,
        exit_with_last_app: a.exit_with_last_app,
        app_env: Vec::new(),
        clear_data: false,
        data_root: None,
    };
    // no `--display`: atld reads the target's own monitor, as on the product's host (host_display); this host's monitor is not the target's
    let (rt, foreign) = runtime(root, &args, &files)?;
    // the run's APK is bundled: its copy under the out dir, as the record and the bundle take only the out, java and tree roots
    let files = bundled(&rt.out.join("run-apk").join("package"), files)?;
    let rt = match &a.display {
        Some(display) => rt.with_display(display.clone()),
        None => rt,
    }
    .with_desktop_env(&a.env);
    let work = rt.out.join("run-apk");
    std::fs::create_dir_all(&work)?;
    let m = package(&rt, &files, pin.as_ref(), &a.target)?;
    let stem = files[0]
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("apk")
        .to_owned();
    let run = run_of(&work, files, &m, &args.log_tags);
    // the record first: the bundle is what the run reads (remote.rs `Target::of`)
    rt.record_inputs(&run)?;
    let target = remote::Target::of(root, &rt.out, "run-apk", &a.host, a.brk)?;
    let stem = if test_apks::names_row(&a.target) {
        a.target.clone()
    } else {
        stem
    };
    let logs = rt
        .out
        .join(remote::REMOTE_DIR)
        .join(target.log_dir())
        .join("run-apk")
        .join(&stem);
    let script = Duration::from_millis(a.pointer.as_deref().map_or(0, script_wait_ms));
    let limit = window::LIMIT + Duration::from_secs(a.hold) + script;
    if !a.apply {
        println!("{}", target.summary());
        println!(
            "dry run: pushes the bundle to {}, checks {} ELF files there, then runs {} with atld's limit at {} s (--hold {} s after the resume); logs to {}; --apply runs it",
            target.target(),
            target.elf_count(),
            m.name,
            limit.as_secs(),
            a.hold,
            logs.display()
        );
        if observe_on {
            println!(
                "observe: on, shots at {shots:?} ms after the app's first present; --apply records it"
            );
        } else {
            println!("observe: off (--observe or --shot turns it on)");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let problems = target.deliver("run-apk")?;
    if !problems.is_empty() {
        println!("{}", verdict::gate("remote run-apk", &problems));
        return Ok(ExitCode::FAILURE);
    }
    // what an earlier run left here is not this run's: a run that ends before its logs return reads none of it
    let _ = std::fs::remove_dir_all(&logs);
    std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
    for name in END_FILES {
        let _ = std::fs::remove_file(run.scratch.join(name));
    }
    // the window needs a display: the session's, else labwc nested in its X11 session (remote.rs Display)
    let (display, why) = remote::probe_display(&a.host)?;
    ensure!(
        display != remote::Display::Headless,
        "remote run-apk: the target has no display for the window: {why}"
    );
    println!("remote run-apk: display: {why}");
    let rt = rt.with_site(target.run_site(limit, logs.join("transcript.log"), display));
    // before the transfer: the run's root is cleared on the target then, so a present line the watch reads is this run's
    let mut watch = if observe_on {
        let plan = observe::Plan {
            base: target.base(),
            root: remote::rel_out(&rt.out, &run.root)?,
            app_log: remote::rel_out(&rt.out, &run.log(&m.name))?,
            shots_ms: shots.clone(),
        };
        Some(observe::Watch::start(
            |script: &str, bound: Duration| target.run_bounded(script, bound),
            plan,
            &logs,
        )?)
    } else {
        None
    };
    let mut atld = rt.start_atld(&run)?;
    let waited = wait_transfer(&mut atld, limit + TRANSFER_MARGIN, || {
        if let Some(w) = watch.as_mut() {
            w.poll();
        }
    });
    if let Some(w) = watch {
        w.finish();
    }
    let transfer = match waited? {
        Some(status) => rt.check_end(status, &run).map_err(|e| format!("{e:#}")),
        None => Err(format!(
            "the transfer to {} did not end within {} s and was stopped",
            target.label(),
            (limit + TRANSFER_MARGIN).as_secs()
        )),
    };
    let watch_ms = a.hold * MS_PER_S;
    let app_log = run.log(&m.name);
    let services_log = run.log("services");
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let app = read(&app_log);
    let services = read(&services_log);
    let outcome = transfer.and_then(|()| {
        let (rc, end_ms) = end_record(&run.scratch)?;
        let ending = Ending {
            complete: watch_complete(&services, watch_ms, end_ms),
            atld_exited: !stopped_at_limit(rc),
            end_ms,
            app_end: app_end(&services, &read(&run.scratch.join("atld.log"))),
        };
        check_delivery(foreign.as_ref(), &m.name, &app)?;
        judge_ending(&services, &app, &ending, watch_ms, a.pointer.is_some())
    });
    let outcome = outcome.and_then(|ok| {
        display_node_check(&read(&run.scratch.join("atld.log"))).map(|gpu| format!("{ok}; {gpu}"))
    });
    copy_logs(&run, &logs)?;
    println!(
        "remote run-apk: transcript {}",
        logs.join("transcript.log").display()
    );
    Ok(finish(
        outcome,
        &app,
        &services,
        &logs.join("atld.log"),
        &logs,
    ))
}

/// Index into `supported` of the ABI AOSP's `NativeLibraryHelper.findSupportedAbi` picks: the lowest index any `lib/<abi>/<file>.so` entry matches. The harness's own reading of the zip, needed before the
/// run to stage a guest; the install itself runs AOSP's.
fn find_supported_abi<'a>(
    entries: impl IntoIterator<Item = &'a str>,
    supported: &[SupportedAbi],
) -> Option<usize> {
    entries
        .into_iter()
        .filter_map(|e| library_entry(e).map(|(abi, _)| abi))
        .filter_map(|abi| supported.iter().position(|s| s.abi == abi))
        .min()
}

/// `(abi, file)` of an APK entry `lib/<abi>/<file>.so` with one path component in `file`.
fn library_entry(entry: &str) -> Option<(&str, &str)> {
    let rest = entry.strip_prefix(LIB_DIR_NAME)?.strip_prefix('/')?;
    let (abi, file) = rest.split_once('/')?;
    (!abi.is_empty() && !file.contains('/') && file.ends_with(".so") && file != ".so")
        .then_some((abi, file))
}

/// The app's ABI when its ISA is foreign (not `host`): the device ABI the install picks over every file of the package ([`find_supported_abi`]; a config split carries the libraries), whose
/// guest and engine the test image then needs. Empty for a host-ISA or no-native-code app.
fn foreign_abi(
    abis: &[SupportedAbi],
    host: &str,
    files: &[PackageFile],
) -> Result<Option<SupportedAbi>> {
    let mut entries = String::new();
    for f in files {
        let list = Command::new("unzip")
            .arg("-Z1")
            .arg(&f.path)
            .output()
            .context("running unzip")?;
        ensure!(
            list.status.success(),
            "unzip -Z1 {}: {}",
            f.path.display(),
            list.status
        );
        entries.push_str(&String::from_utf8_lossy(&list.stdout));
    }
    Ok(find_supported_abi(entries.lines(), abis)
        .map(|i| abis[i].clone())
        .filter(|a| a.isa != host))
}

/// The staged runtime of a run of `args` for the package `files`, [`configure`]d for the package's foreign ABI, which is returned for the delivery check.
fn runtime(
    root: &Path,
    args: &Args,
    files: &[PackageFile],
) -> Result<(Runtime, Option<SupportedAbi>)> {
    let rt = Runtime::new(
        root,
        "run-apk",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        STAGE,
        args.verbose,
    )?;
    let foreign = foreign_abi(rt.abis(), &args.host, files)?;
    let rt = configure(root, args, rt, foreign.as_ref())?;
    Ok((rt, foreign))
}

/// The run-apk configuration of the staged runtime `rt` for `args` and the APKs' foreign ABI `foreign`: the window gate's staging with the GL driver, the native bridge guest of the host (identity hosts only) and the GTK UI; for a foreign ISA, that ISA's guest and QEMU engine; then `args`' toggles.
/// The builders accumulate (`with_native_bridge_guest` extends its list): configure a staged runtime once.
pub(crate) fn configure(
    root: &Path,
    args: &Args,
    rt: Runtime,
    foreign: Option<&SupportedAbi>,
) -> Result<Runtime> {
    let rt = rt.with_gralloc(root)?.with_gl_driver(root)?;
    // the host ISA's guest exists only where its identity engine does, as app.rs; elsewhere the guest is foreign (below)
    let rt = if crate::arches::identity(root, &args.host)? {
        rt.with_native_bridge_guest(&args.host)?
    } else {
        rt
    };
    let rt = rt.with_ui("gtk");
    let rt = match foreign {
        Some(a) => {
            crate::build_state::foreign_out(root, &rt.out, &args.host, &a.isa, "run-apk")?;
            rt.with_native_bridge_guest(&a.isa)?
                .with_qemu_engine(root, &a.isa)?
        }
        None => rt,
    };
    let rt = if args.no_app_seccomp {
        rt.without_app_seccomp()
    } else {
        rt
    };
    let rt = if args.break_isa {
        rt.with_broken_instruction_set()
    } else {
        rt
    };
    let rt = if args.break_render_path {
        rt.with_broken_render_path()
    } else {
        rt
    };
    let rt = if args.break_webview_engine {
        rt.without_webview_engine()?
    } else {
        rt
    };
    let rt = match &args.pointer {
        Some(script) => rt.with_headless_pointer(script),
        None => rt,
    };
    let rt = if args.tilt_keys {
        rt.with_tilt_keys(&[])
    } else {
        rt
    };
    let rt = if args.mouse_touch {
        rt.with_mouse_touch()
    } else {
        rt
    };
    let rt = if args.esc_back {
        rt.with_esc_back()
    } else {
        rt
    };
    let rt = if args.exit_with_last_app {
        rt.with_exit_with_last_app()
    } else {
        rt
    };
    Ok(rt.with_app_env(&args.app_env))
}

/// The foreign ABI the APKs `apks` share ([`foreign_abi`] of each): one configured runtime serves them all, so they must agree.
pub(crate) fn shared_foreign(
    rt: &Runtime,
    host: &str,
    apks: &[&Path],
) -> Result<Option<SupportedAbi>> {
    let mut per_apk = Vec::with_capacity(apks.len());
    for apk in apks {
        let file = PackageFile {
            path: apk.to_path_buf(),
        };
        per_apk.push(foreign_abi(rt.abis(), host, &[file])?);
    }
    if let Some(first) = per_apk.first() {
        for (apk, found) in apks.iter().zip(&per_apk) {
            ensure!(
                found == first,
                "{}: foreign ABI {found:?} differs from {first:?}: one runtime serves every APK of a gate",
                apk.display()
            );
        }
    }
    Ok(per_apk.into_iter().next().flatten())
}

/// Whether `line` is the bridge's load line (atl-native-bridge loader.rs) for a library of `package` and ABI `a`: extracted, `/data/app/<package>/lib/<isa>/` (PackageManager's
/// `lib/<instruction set>` dir), or straight from an APK (extractNativeLibs=false), `/data/app/<package>/<apk>!/lib/<abi>/` (the zip entry dir).
fn is_delivery_line(line: &str, package: &str, a: &SupportedAbi) -> bool {
    let prefix = format!("atl-native-bridge: loaded /data/app/{package}/");
    let Some(path) = line.split_once(&prefix).map(|(_, p)| p) else {
        return false;
    };
    path.starts_with(&format!("lib/{}/", a.isa))
        || path.split_once(".apk!/").is_some_and(|(apk, entry)| {
            !apk.contains('/') && entry.starts_with(&format!("lib/{}/", a.abi))
        })
}

/// The delivery check of a foreign-ISA app (`foreign`): the app log `app` has the bridge loading one of the package's libraries of that ABI into the guest, so ART handed the bridge the app's ISA and the engine took the library.
/// Checked before the window verdict: without delivery nothing the app's native code does is the guest's.
fn check_delivery(foreign: Option<&SupportedAbi>, package: &str, app: &str) -> Result<(), String> {
    let Some(a) = foreign else {
        return Ok(());
    };
    let isa = &a.isa;
    let line = format!(
        "atl-native-bridge: loaded /data/app/{package}/(lib/{isa}|<apk>!/lib/{})/",
        a.abi
    );
    match app.lines().find(|l| is_delivery_line(l, package, a)) {
        Some(l) => {
            println!("run-apk: guest delivery {isa}: ok ({})", l.trim());
            Ok(())
        }
        None => {
            println!("run-apk: guest delivery {isa}: FAIL (no `{line}` line in the app log)");
            Err(format!(
                "guest delivery {isa}: no library of lib/{isa} loaded into the guest"
            ))
        }
    }
}

/// Reads the base APK's manifest (`files`, base first; atld stages the files themselves). A row's pin is checked against the manifest. Prints the package's line, and that it is unpinned when no row names it.
fn package(
    rt: &Runtime,
    files: &[PackageFile],
    pin: Option<&test_apks::Row>,
    target: &str,
) -> Result<Manifest> {
    let apk_file = &files.first().context("a package without files")?.path;
    let m = manifest::read(&rt.apk.aapt2, apk_file)?;
    if let Some(row) = pin {
        test_apks::check_identity(
            target,
            row,
            &m.name,
            m.version_code,
            m.version_name.as_deref(),
        )?;
    }
    // the launcher is the services' (the installer's parse, aliases included): aapt2's first launcher activity is only shown
    let activity = m.launch_activity().map_or("none", |a| a.name.as_str());
    println!(
        "run-apk: {} ({} versionCode {}, sdk {}..{}, aapt2 launcher activity {activity}, {} file(s))",
        apk_file.display(),
        m.name,
        m.version_code,
        m.min_sdk,
        m.target_sdk,
        files.len()
    );
    if pin.is_none() {
        println!(
            "run-apk: UNPINNED: {} is not a row of {}; no sha256, size or signer check ran",
            apk_file.display(),
            test_apks::FILE
        );
    }
    Ok(m)
}

/// The run's dirs and its one package, started at its launcher activity.
fn run_of(work: &Path, files: Vec<PackageFile>, m: &Manifest, log_tags: &str) -> Run {
    Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![Package { files }],
        libraries: vec![],
        shell: None,
        log_tags: log_tags.to_owned(),
    }
}

/// Waits for the transfer that runs the app on the target: its status, or `None` once `limit` has passed (the transfer is then killed). `tick` runs between the status checks.
pub(crate) fn wait_transfer(
    child: &mut Child,
    limit: Duration,
    mut tick: impl FnMut(),
) -> Result<Option<ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            child.wait()?;
            return Ok(None);
        }
        tick();
        std::thread::sleep(window::POLL);
    }
}

/// The target's end record of a remote run (remote.rs `Plan::script`): atld's exit status (`timeout`'s, when the limit ended it) and the end's time in ms on the target's clock.
pub(crate) fn end_record(scratch: &Path) -> Result<(i32, u64), String> {
    let read = |name: &str| {
        std::fs::read_to_string(scratch.join(name))
            .map(|text| text.trim().to_owned())
            .map_err(|_| format!("the target returned no {name}: see the transcript"))
    };
    let rc = read("atld.rc")?
        .parse::<i32>()
        .map_err(|_| "atld.rc is not an exit code".to_owned())?;
    let end_ms = read("end.ms")?
        .parse::<u64>()
        .map_err(|_| "end.ms is not a time".to_owned())?;
    Ok((rc, end_ms))
}

/// Whether the target's `timeout` ended atld: 124 when the limit did, 137 when its KILL did (timeout(1)). Any other status is atld's own end.
pub(crate) fn stopped_at_limit(rc: i32) -> bool {
    matches!(rc, 124 | 137)
}

/// Whether a remote run's watch ran to its end: the target's end came `watch_ms` after the last resume. Both times are on the target's clock.
fn watch_complete(services: &str, watch_ms: u64, end_ms: u64) -> bool {
    last_resumed(services).is_some_and(|r| end_ms >= r.ms + watch_ms)
}

/// How a run ended, as the judge reads it: the watch ran to its end, atld ended before its end by itself, the end's time on the clock the logs carry (this host's, or the target's), and
/// the app's own normal end when it had one ([`app_end`]).
struct Ending {
    complete: bool,
    atld_exited: bool,
    end_ms: u64,
    app_end: Option<&'static str>,
}

/// The services' line when an app process's death left no activity anywhere (atl-services activity.rs `app_died`): `pid <n> finished|live`.
const LAST_APP_LINE: &str = "services: last app process ended: pid ";
/// The services' line for a desktop close they took (registry_service.rs `remove_task`).
const REMOVE_TASK_LINE: &str = "services: removeTask task ";

/// The app's normal end, from the services' last-app line: `finished` (no live activity at its death) is the window's close when the services took a removeTask, else the app
/// finishing its last activity; `live` is normal only when atld saw its process exit 0 (its own exit after a quit dialog). A crash, or a signal while an activity was live, is None.
/// AOSP reports a crash only from handleApplicationCrash or a native crash; a self-kill (Process.killProcess) after the last activity finished is no crash.
fn app_end(services: &str, atld_log: &str) -> Option<&'static str> {
    let rest = services
        .lines()
        .find_map(|l| l.split_once(LAST_APP_LINE).map(|(_, r)| r))?;
    let mut words = rest.split_whitespace();
    let pid = words.next()?;
    match words.next()? {
        "finished" if services.contains(REMOVE_TASK_LINE) => Some("window closed"),
        "finished" => Some("app finished its last activity"),
        "live" => {
            let exited_0 = format!("process {pid} ended: exit status: 0");
            atld_log
                .lines()
                .any(|l| l.trim_end().ends_with(&exited_0))
                .then_some("app exited 0")
        }
        _ => None,
    }
}

/// The verdict of a run that ended: [`judge`], with the fault window widened to the run's end when the run was scripted.
fn judge_ending(
    services: &str,
    app: &str,
    ending: &Ending,
    watch_ms: u64,
    scripted: bool,
) -> Result<String, String> {
    let fault_ms = match (scripted, last_resumed(services)) {
        (true, Some(r)) => watch_ms.max(ending.end_ms.saturating_sub(r.ms)),
        _ => watch_ms,
    };
    judge(
        services,
        app,
        ending.complete,
        ending.atld_exited,
        fault_ms,
        ending.app_end,
    )
}

/// Copies a run's logs to `to`: the app's and the services' logs (`root/log`), and atld's log and end record (`END_FILES`). Files the run did not write are skipped.
fn copy_logs(run: &Run, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let mut from: Vec<PathBuf> = std::fs::read_dir(run.root.join("log"))
        .map(|dir| dir.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    from.extend(END_FILES.iter().map(|name| run.scratch.join(name)));
    for path in from.into_iter().filter(|p| p.is_file()) {
        let name = path.file_name().context("a log without a file name")?;
        std::fs::copy(&path, to.join(name))
            .with_context(|| format!("copying {}", path.display()))?;
    }
    Ok(())
}

/// `KEY=VALUE` of `--env`.
fn parse_env(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
        _ => Err(format!("`{s}`: want KEY=VALUE")),
    }
}

/// A remote target's desktop with a DRM render node renders on a GPU (the gate's targets are real desktops; a software-only desktop is not one), so its Wayland EGL display must report that node: atld's `gpu:` line must be the display tier.
/// The trial tier there means the probe lost the node (gpu-selection.md G1 break). Ok: atld's `gpu:` line.
fn display_node_check(atld_log: &str) -> Result<String, String> {
    let gpu = atld_log
        .lines()
        .find_map(|l| l.strip_prefix("atld: gpu: "))
        .ok_or("atld logged no `gpu:` line")?;
    let render_node = atld_log
        .lines()
        .any(|l| l.starts_with("atld: gpu-probe: devices") && l.contains("/dev/dri/renderD"));
    if render_node && !gpu.contains("(display, ") {
        return Err(format!(
            "the target has a render node but its display reported none: gpu: {gpu}"
        ));
    }
    Ok(format!("gpu: {gpu}"))
}

/// atld's render-path line (`gpu-render: path <module|baseline> (...)`, atld main.rs): a kernel-offered candidate (atl-broker gpu::RENDER_PATHS) must be taken; baseline is red only when one was offered and its probe failed or rendered on the baseline's renderer.
/// Ok: the path, plus the driver's `buffer tier` line from the app log. No line (headless run, no gralloc): Ok, nothing to judge.
fn render_path_check(atld_log: &str, app: &str) -> Result<String, String> {
    let Some(path) = atld_log
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("atld: gpu-render: path "))
    else {
        return Ok("render path: none probed".to_owned());
    };
    let tier = app
        .lines()
        .rev()
        .find_map(|l| l.split_once("libEGL_atl: buffer tier ").map(|(_, t)| t))
        .unwrap_or("not logged");
    let offered_not_taken = path.starts_with("baseline (")
        && !path.starts_with("baseline (no kernel render path offered");
    if offered_not_taken {
        Err(format!("render path offered but not taken: {path}"))
    } else {
        Ok(format!(
            "render path {path}; buffer tier {tier}; {}",
            crate::gdk_dmabuf::present_tier(app)
        ))
    }
}

/// A line of one of atld's summary blocks (print_summary): `atld: summary ...` (the API block) or `atld: <family>-summary ...` (one per atl_diag family), so a new family is forwarded without a change here.
fn is_summary_line(line: &str) -> bool {
    line.strip_prefix("atld: ")
        .and_then(|rest| rest.split(' ').next())
        .is_some_and(|word| word == "summary" || word.ends_with("-summary"))
}

/// The run's printed verdict: the pass line, or the failure with the app's and the services' log tails. Then atld's end lines and its summary block, and where the logs are.
fn finish(
    outcome: Result<String, String>,
    app: &str,
    services: &str,
    atld_log: &Path,
    log_dir: &Path,
) -> ExitCode {
    match &outcome {
        Ok(s) => println!("run-apk: ok {s}"),
        Err(s) => {
            println!("run-apk: FAILED {s}");
            println!("---- app log tail\n{}", window::tail(app, window::TAIL));
            println!(
                "---- services log tail\n{}",
                window::tail(services, window::TAIL)
            );
        }
    }
    // the app's end as its parent saw it (a signal leaves no line of its own in the app's or the services' log), then every block of atld's ranked outcomes (atld main.rs print_summary)
    for line in std::fs::read_to_string(atld_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(" ended: ") || l.starts_with("atld: log ") || is_summary_line(l))
    {
        println!("run-apk: {line}");
    }
    // the GL driver of each app process and its rule (atl-services activity attach)
    for line in services
        .lines()
        .filter(|l| l.starts_with("services: gl-driver "))
    {
        println!("run-apk: {line}");
    }
    for line in fps_summary(app) {
        println!("run-apk: {line}");
    }
    println!("run-apk: logs {}", log_dir.display());
    if outcome.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The composer's per-second `composer: fps` lines (atl-composer frame_rate.rs) summarised per buffer size, in first-seen order: `fps buffer=WxH seconds=<n> mean=<f> min=<n> p5=<n>`.
/// p5 is nearest-rank (the ceil(0.05 n)-th smallest second): an observed second, never interpolated.
fn fps_summary(app: &str) -> Vec<String> {
    let mut sizes: Vec<(String, Vec<u64>)> = Vec::new();
    for line in app.lines().filter(|l| l.contains("composer: fps ")) {
        let (Some(n), Some(buffer)) = (
            crate::composer::field(line, "presented"),
            line.split_whitespace()
                .find_map(|w| w.strip_prefix("buffer=")),
        ) else {
            continue;
        };
        match sizes.iter_mut().find(|(b, _)| b == buffer) {
            Some((_, v)) => v.push(n),
            None => sizes.push((buffer.to_owned(), vec![n])),
        }
    }
    sizes
        .into_iter()
        .map(|(buffer, mut v)| {
            v.sort_unstable();
            let rank = (v.len() * 5).div_ceil(100).max(1);
            let mean = v.iter().sum::<u64>() as f64 / v.len() as f64;
            format!(
                "fps buffer={buffer} seconds={} mean={mean:.1} min={} p5={}",
                v.len(),
                v[0],
                v[rank - 1]
            )
        })
        .collect()
}

/// `files` copied into `dir` under their own names: the copies are the run's inputs, so a remote run's bundle takes them.
fn bundled(dir: &Path, files: Vec<PackageFile>) -> Result<Vec<PackageFile>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    files
        .into_iter()
        .map(|f| {
            let name = f.path.file_name().context("an APK without a file name")?;
            let to = dir.join(name);
            std::fs::copy(&f.path, &to)
                .with_context(|| format!("copying {} to {}", f.path.display(), to.display()))?;
            Ok(PackageFile { path: to })
        })
        .collect()
}

/// The package to run, base first: a row's pinned files (verified by `test_apks::ensure`), or the one base APK a path names, which no row pins.
fn source(root: &Path, target: &str) -> Result<(Option<test_apks::Row>, Vec<PackageFile>)> {
    if test_apks::names_row(target) {
        let (row, files) = test_apks::ensure(root, target)?;
        let files = files
            .into_iter()
            .map(|(_, path)| PackageFile { path })
            .collect();
        return Ok((Some(row), files));
    }
    let path = PathBuf::from(target);
    ensure!(
        path.is_file(),
        "{target}: not a file (an argument holding `.`, `/` or `\\` is a path)"
    );
    Ok((None, vec![PackageFile { path }]))
}

/// The script's `wait:MS` items summed: how long it runs after the first present (a bad item is the app's to refuse, counted 0 here).
fn script_wait_ms(script: &str) -> u64 {
    script
        .split_whitespace()
        .filter_map(|item| item.strip_prefix("wait:")?.parse::<u64>().ok())
        .sum()
}

/// Frames the app presented at or after `from_ms`: its present lines stamped no earlier (a line's stamp is its first field).
fn presented_since(app_log: &str, from_ms: u64) -> usize {
    app_log
        .lines()
        .filter(|l| l.contains(PRESENT_LINE))
        .filter(|l| {
            l.split_whitespace()
                .next()
                .and_then(parse_stamp)
                .is_some_and(|t| t >= from_ms)
        })
        .count()
}

/// Where the frames stopped, from the composer's last counters line: buffers latched from the app, scenes published, and scenes the window took (each take is one present line).
/// Buffers without scenes = the clock never composed; scenes without takes = the window's frame clock never took one (unmapped, or not ticking).
fn scene_counts(app: &str) -> String {
    let Some(line) = crate::composer::last_counters(app) else {
        return "no `composer: counters` line either".to_owned();
    };
    let n = |key| crate::composer::field(line, key).map_or("?".to_owned(), |v| v.to_string());
    format!(
        "composer: buffers_acquired={} scenes={} scenes_taken={} scenes_dropped={} presents_refused={}",
        n("buffers_acquired"),
        n("scenes"),
        n("scenes_taken"),
        n("scenes_dropped"),
        n("presents_refused")
    )
}

/// The run's verdict over its two logs, about the last activity the app resumed: the services are settled (nothing pending after that resume), no fault in the app log within
/// `watch_ms` after it (`atld_exited` is a fault at the window's end), the watch was run to its end (`complete`), and a frame was presented after it.
/// `app_end` ([`app_end`]): the app ended normally, so the run may end early, atld's exit is that end, and a self-kill line is no fault (a FATAL still is). Ok: the pass line, naming that end; Err: the first failed part.
fn judge(
    services: &str,
    app: &str,
    complete: bool,
    atld_exited: bool,
    watch_ms: u64,
    app_end: Option<&str>,
) -> Result<String, String> {
    let no_self_kill;
    let (app, atld_exited) = match app_end {
        Some(_) => {
            no_self_kill = app
                .lines()
                .filter(|l| !l.contains("Sending signal"))
                .collect::<Vec<_>>()
                .join("\n");
            (no_self_kill.as_str(), false)
        }
        None => (app, atld_exited),
    };
    let Some(last) = last_resumed(services) else {
        return Err(
            "no activity resumed: the services log has no `services: activity resumed` line"
                .to_owned(),
        );
    };
    if !settled(services) {
        return Err(format!(
            "not settled after {}: a start or lifecycle step is still unacknowledged",
            last.class
        ));
    }
    if let Some(fault) = window::fault_after(app, last.ms, watch_ms, last.ms, atld_exited) {
        return Err(format!("{fault} (last resumed: {})", last.class));
    }
    if !complete && app_end.is_none() {
        return Err(format!(
            "the run ended before the {watch_ms} ms watch after {} was over",
            last.class
        ));
    }
    let frames = presented_since(app, last.ms);
    if frames == 0 {
        return Err(format!(
            "no frame presented after {} resumed: the app log has no `{PRESENT_LINE}` line after it; {}",
            last.class,
            scene_counts(app)
        ));
    }
    let ended = app_end.map_or_else(String::new, |e| format!("; ended: {e}"));
    Ok(format!(
        "onResume of {} at {}, {frames} frame(s) presented after it, settled, no fault in the {watch_ms} ms after it{ended}",
        last.class,
        window::stamp_text(last.ms)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atl-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Old build-dir device, no user device yet: moved whole, old gone.
    #[test]
    fn build_dir_device_moves_to_an_absent_user_device() {
        let base = scratch("move");
        let old = base.join("out/run-apk/device");
        let new = base.join("share/atl/device");
        std::fs::create_dir_all(old.join("data/user/0/p/files")).unwrap();
        std::fs::write(old.join("data/user/0/p/files/save.txt"), "s").unwrap();
        migrate_build_dir_device(&old, &new).unwrap();
        assert_eq!(
            std::fs::read_to_string(new.join("data/user/0/p/files/save.txt")).unwrap(),
            "s"
        );
        assert!(!old.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Both exist: nothing moved, merged or deleted.
    #[test]
    fn existing_user_device_leaves_both_untouched() {
        let base = scratch("keep");
        let old = base.join("old");
        let new = base.join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("a"), "old").unwrap();
        std::fs::write(new.join("b"), "new").unwrap();
        migrate_build_dir_device(&old, &new).unwrap();
        assert_eq!(std::fs::read_to_string(old.join("a")).unwrap(), "old");
        assert!(!new.join("a").exists());
        assert_eq!(std::fs::read_to_string(new.join("b")).unwrap(), "new");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// No old device: nothing created.
    #[test]
    fn no_build_dir_device_is_a_no_op() {
        let base = scratch("none");
        migrate_build_dir_device(&base.join("old"), &base.join("new")).unwrap();
        assert!(!base.exists());
    }

    /// Every print_summary block is forwarded (API, prop, a future family); other atld lines are not.
    #[test]
    fn summary_lines_of_every_family() {
        for yes in [
            "atld: summary 3 keys",
            "atld: prop-summary 173 keys",
            "atld: prop-summary prop:miss apex.all.ready x>=8 +0.000s",
            "atld: fs-summary 1 keys",
        ] {
            assert!(is_summary_line(yes), "{yes}");
        }
        for no in [
            "atld: control: /x/control.sock",
            "atld: summaryx 1",
            "run-apk: atld: summary 1",
        ] {
            assert!(!is_summary_line(no), "{no}");
        }
    }

    /// Two sizes: 121x175 seconds 45,44,46 -> mean 135/3 = 45.0, min 44, p5 rank ceil(0.15)=1 -> 44; 447x745 seconds 9,8 -> mean 8.5, min 8, p5 8. A line without `buffer=` is skipped.
    #[test]
    fn fps_summary_per_buffer_size() {
        let app = "\
1.0 INFO composer: fps presented=45 taken=46 buffer=121x175 window=480x800
2.0 INFO composer: fps presented=44 taken=44 buffer=121x175 window=480x800
2.5 INFO composer: fps presented=7
3.0 INFO composer: fps presented=9 taken=9 buffer=447x745 window=480x800
4.0 INFO composer: fps presented=46 taken=46 buffer=121x175 window=480x800
5.0 INFO composer: fps presented=8 taken=8 buffer=447x745 window=480x800
";
        assert_eq!(
            fps_summary(app),
            [
                "fps buffer=121x175 seconds=3 mean=45.0 min=44 p5=44",
                "fps buffer=447x745 seconds=2 mean=8.5 min=8 p5=8",
            ]
        );
    }

    fn abis(list: &[(&str, &str)]) -> Vec<SupportedAbi> {
        list.iter()
            .map(|(abi, isa)| SupportedAbi {
                abi: (*abi).into(),
                isa: (*isa).into(),
            })
            .collect()
    }

    #[test]
    fn the_most_preferred_device_abi_wins_whatever_the_entry_order() {
        let s = abis(&[("x86_64", "x86_64"), ("x86", "x86"), ("arm64-v8a", "arm64")]);
        let entries = [
            "classes.dex",
            "lib/arm64-v8a/libg.so",
            "lib/x86/libg.so",
            "lib/mips/libg.so",
        ];
        assert_eq!(find_supported_abi(entries, &s), Some(1));
    }

    #[test]
    fn only_direct_so_entries_count() {
        let s = abis(&[("x86_64", "x86_64")]);
        for e in [
            "lib/x86_64/sub/libg.so",
            "lib/x86_64/readme.txt",
            "lib/x86_64/",
            "assets/lib/x86_64/libg.so",
            "lib//libg.so",
        ] {
            assert_eq!(find_supported_abi([e], &s), None, "{e}");
        }
    }

    /// The load line as loader.rs prints it for CTR's arm64 libctro (measured 2026-11-12); the break is identity's run, which loads nothing into a guest of the app's ISA.
    #[test]
    fn delivery_needs_the_guest_load_of_the_apps_isa() {
        let pkg = "com.zeptolab.ctr.ads";
        let app = "atl-native-bridge: loaded /data/app/com.zeptolab.ctr.ads/lib/arm64/libctro.so into the guest\n";
        let arm64 = &abis(&[("arm64-v8a", "arm64")])[0];
        let riscv64 = &abis(&[("riscv64", "riscv64")])[0];
        assert!(check_delivery(Some(arm64), pkg, app).is_ok());
        assert!(check_delivery(Some(arm64), pkg, "").is_err());
        assert!(check_delivery(Some(riscv64), pkg, app).is_err());
        // a host-ISA app has nothing to deliver
        assert!(check_delivery(None, pkg, "").is_ok());
    }

    /// The load line of an extractNativeLibs=false app: ART passes the zip entry path (bionic `kZipFileSeparator`), the entry dir named by the ABI, not the ISA.
    #[test]
    fn delivery_from_an_apk_entry_names_the_abi() {
        let pkg = "jp.co.ponos.battlecatstw";
        let app = "atl-native-bridge: loaded /data/app/jp.co.ponos.battlecatstw/split_config.arm64_v8a.apk!/lib/arm64-v8a/libnative-lib.so into the guest\n";
        let arm64 = &abis(&[("arm64-v8a", "arm64")])[0];
        let armv7 = &abis(&[("armeabi-v7a", "arm")])[0];
        assert!(check_delivery(Some(arm64), pkg, app).is_ok());
        assert!(check_delivery(Some(armv7), pkg, app).is_err());
        // another package's APK is not this app's delivery
        assert!(check_delivery(Some(arm64), "com.other", app).is_err());
    }

    /// Lines as atld prints them (atld main.rs `run`): a GPU desktop, its display tier, the trial tier, and WSLg's vgem-only devices.
    #[test]
    fn render_path_check_reds_an_offer_not_taken() {
        let app = "libEGL_atl: buffer tier readback (x)\n";
        let taken = "atld: gpu-render: path dxgkrnl (renderer D3D12 (GPU), OpenGL ES 3.1 Mesa)\n";
        assert!(
            render_path_check(taken, app)
                .unwrap()
                .contains("buffer tier readback")
        );
        let none = "atld: gpu-render: path baseline (no kernel render path offered)\n";
        assert!(render_path_check(none, app).is_ok());
        assert!(render_path_check("", app).is_ok());
        // what --break-render-path leaves: the candidate's probe failed
        let broken = "atld: gpu-render: path baseline (dxgkrnl: probe exited exit status: 1: gpu-renderer: FAIL eglInitialize)\n";
        assert!(render_path_check(broken, app).is_err());
    }

    #[test]
    fn display_node_check_reds_a_lost_node() {
        let probe = "atld: gpu-probe: devices [\"/dev/dri/card0\", \"/dev/dri/renderD128\"]\n";
        let display = "atld: gpu: node /dev/dri/renderD128 (display, renderer Mesa Intel(R) Graphics (LNL))\n";
        let trial = "atld: gpu: display reports no node (renderer llvmpipe); allocator node /dev/dri/renderD128 by trial\n";
        assert!(display_node_check(&format!("{probe}{display}")).is_ok());
        let e = display_node_check(&format!("{probe}{trial}")).unwrap_err();
        assert!(e.contains("reported none"), "{e}");
        // no render node shown to the probe: the trial tier is the display's real answer
        assert!(
            display_node_check(&format!(
                "atld: gpu-probe: devices [\"/dev/dri/card0\"]\n{trial}"
            ))
            .is_ok()
        );
        assert!(display_node_check(probe).is_err());
    }

    const RESUME: &str =
        "services: activity resumed org.x.Main at=100.000\nservices: activity settled at=100.001\n";
    const PRESENT: &str = "100.900 D atl-composer: ui: present n=1 time_ns=1\n";

    #[test]
    fn judge_passes_a_resumed_presenting_app() {
        let app = format!("100.500 I tag: onCreate\n{PRESENT}");
        assert!(judge(RESUME, &app, true, false, 2_000, None).is_ok());
    }

    #[test]
    fn judge_fails_each_missing_or_broken_part() {
        let err = |services: &str, app: &str, complete: bool, atld_exited: bool| {
            judge(services, app, complete, atld_exited, 2_000, None).unwrap_err()
        };
        assert!(err("", PRESENT, true, false).contains("no activity resumed"));
        assert!(err(RESUME, "100.500 I tag: x\n", true, false).contains("no frame presented"));
        let crashed = format!("{PRESENT}100.700 E AndroidRuntime: FATAL EXCEPTION: main\n");
        assert!(err(RESUME, &crashed, true, false).contains("FATAL EXCEPTION"));
        assert!(err(RESUME, PRESENT, true, true).contains("atld exited"));
        assert!(err(RESUME, PRESENT, false, false).contains("ended before"));
        let pending = "services: activity resumed org.x.Main at=100.000\nservices: activity unsettled at=100.500\n";
        assert!(err(pending, PRESENT, true, false).contains("not settled"));
    }

    /// The shape of the r5 hold run: Splash resumes and presents, Main resumes 39 s later and dies 67 ms after that. Splash's watch is long over by then; the verdict is about Main.
    #[test]
    fn judge_checks_the_last_resumed_activity_of_the_hold_run() {
        let services = "services: activity resumed org.fossify.math.activities.SplashActivity at=1793887559.475\n\
                        services: activity unsettled at=1793887559.600\n\
                        services: activity resumed org.fossify.math.activities.MainActivity at=1793887598.643\n\
                        services: activity settled at=1793887598.644\n";
        let app = "1793887559.900 D atl-composer: ui: present n=1 time_ns=1\n\
                   1793887598.710 E AndroidRuntime: FATAL EXCEPTION: main\n";
        let err = judge(services, app, true, false, 2_000, None).unwrap_err();
        assert!(
            err.contains("FATAL EXCEPTION") && err.contains("MainActivity"),
            "{err}"
        );
    }

    /// A second activity that starts after the first watch and presents nothing is not a pass: the verdict is about it, and it has no frame.
    #[test]
    fn judge_refuses_a_second_activity_without_a_frame() {
        let services = "services: activity resumed org.x.Splash at=100.000\n\
                        services: activity unsettled at=103.000\n\
                        services: activity resumed org.x.Main at=103.500\n\
                        services: activity settled at=103.501\n";
        let err = judge(services, PRESENT, true, false, 2_000, None).unwrap_err();
        assert!(err.contains("no frame presented after org.x.Main"), "{err}");
    }

    /// The tr-try5 shape: the composer latched buffers and published scenes the window never took. The failure names those counts, so it reads as "the window took none", not "the app drew none".
    #[test]
    fn no_frame_failure_names_the_composer_scene_counts() {
        let app = "100.900  INFO composer: counters ticks=9 buffers_acquired=90159 scenes=90163 scenes_taken=0 scenes_dropped=90161 presents_refused=0\n";
        let err = judge(RESUME, app, true, false, 2_000, None).unwrap_err();
        assert!(
            err.contains("buffers_acquired=90159 scenes=90163 scenes_taken=0 scenes_dropped=90161"),
            "{err}"
        );
        let err = judge(RESUME, "100.500 I tag: x\n", true, false, 2_000, None).unwrap_err();
        assert!(err.contains("no `composer: counters` line"), "{err}");
    }

    #[test]
    fn script_waits_are_summed() {
        assert_eq!(script_wait_ms("wait:6000 down:1,2 up:1,2 wait:2500"), 8500);
        assert_eq!(script_wait_ms("down:1,2 wait:x"), 0);
    }

    #[test]
    fn judge_ignores_a_fault_after_the_watch() {
        let app = format!("{PRESENT}102.500 E AndroidRuntime: FATAL EXCEPTION: main\n");
        assert!(judge(RESUME, &app, true, false, 2_000, None).is_ok());
    }

    /// A remote run's watch is complete when the target's end came `watch_ms` after the resume (both on the target's clock): RESUME's resume is at 100 000 ms, so a 2 s watch is over at 102 000 ms.
    #[test]
    fn remote_watch_is_complete_at_the_target_end_after_the_watch() {
        assert!(watch_complete(RESUME, 2_000, 102_000));
        assert!(!watch_complete(RESUME, 2_000, 101_999));
        assert!(!watch_complete("", 2_000, 200_000));
    }

    /// timeout(1) exit codes: 124 when the limit ends the command, 137 when its KILL does; any other status is atld's own end.
    #[test]
    fn timeout_codes_mean_the_limit_ended_atld() {
        assert!(stopped_at_limit(124));
        assert!(stopped_at_limit(137));
        assert!(!stopped_at_limit(0));
        assert!(!stopped_at_limit(1));
    }

    /// The remote verdict is run-apk's: a run whose end came before the watch's end fails as "ended before", one whose end is past it passes.
    #[test]
    fn remote_end_is_judged_on_the_watch_it_reached() {
        let ending = |end_ms| Ending {
            complete: watch_complete(RESUME, 2_000, end_ms),
            atld_exited: false,
            end_ms,
            app_end: None,
        };
        assert!(judge_ending(RESUME, PRESENT, &ending(102_000), 2_000, false).is_ok());
        let err = judge_ending(RESUME, PRESENT, &ending(101_000), 2_000, false).unwrap_err();
        assert!(err.contains("ended before"), "{err}");
    }

    /// A run that ended early, atld gone by itself (the play sessions' shapes, play-session.md sessions 3-4), judged as run-apk judges it: the watch is the --hold's 14400 s.
    fn ended_run(services: &str, app: &str, atld_log: &str) -> Result<String, String> {
        let ending = Ending {
            complete: false,
            atld_exited: true,
            end_ms: 0,
            app_end: app_end(services, atld_log),
        };
        judge_ending(services, app, &ending, 14_400_000, false)
    }

    const AB2_UP: &str = "services: activity resumed com.rovio.abba.vkquality.VkQualityActivity at=1794924856.299\n\
                          services: activity settled at=1794924856.299\n";
    const AB2_PRESENT: &str = "1794924857.000 D atl-composer: ui: present n=1 time_ns=1\n";

    /// AB2 (session 4, s4-ab2b): the owner closed the window, the services killed the app after the removal; FAILED `atld exited inside the window` before, now ok.
    #[test]
    fn a_closed_window_is_a_normal_end() {
        let services = format!(
            "{AB2_UP}services: removeTask task 1 of uid 10002\n\
             services: activity remove task 1 wait=true at=1794925013.410\n\
             services: task 1 removed: killing com.rovio.baba pids [89975] at=1794925014.411\n\
             services: activity settled at=1794925014.540\n\
             services: process com.rovio.baba (pid 89975) died\n\
             services: last app process ended: pid 89975 finished at=1794925014.600\n"
        );
        let atld = "atld: process 89975 ended: exit status: 137 (signal 9)\n";
        let ok = ended_run(&services, AB2_PRESENT, atld).expect("a close is a normal end");
        assert!(ok.ends_with("ended: window closed"), "{ok}");
    }

    /// Battle Cats TW (session 4, s4-bctw2): after the close the app's onDestroy kills its own process (`Sending signal ... SIG: 9`); FAILED `the process was killed` before, now ok.
    #[test]
    fn a_self_kill_after_the_close_is_no_fault() {
        let services = "services: activity resumed jp.co.ponos.battlecats.MyActivity at=1794927342.607\n\
                        services: activity settled at=1794927342.607\n\
                        services: removeTask task 1 of uid 10002\n\
                        services: activity settled at=1794927510.346\n\
                        services: process jp.co.ponos.battlecatstw (pid 89339) died\n\
                        services: last app process ended: pid 89339 finished at=1794927510.450\n";
        let app = "1794927343.000 D atl-composer: ui: present n=1 time_ns=1\n\
                   1794927510.398 I liblog/Process: Sending signal. PID: 2 SIG: 9\n";
        let atld = "atld: process 89339 ended: exit status: 137 (signal 9)\n";
        let ok = ended_run(services, app, atld).expect("a self-kill after finish is no crash");
        assert!(ok.ends_with("ended: window closed"), "{ok}");
        // without the services' line (the old log), the same app log is the old FAILED
        let old = services.replace(
            "services: last app process ended: pid 89339 finished at=1794927510.450\n",
            "",
        );
        assert!(
            ended_run(&old, app, atld)
                .unwrap_err()
                .contains("the process was killed")
        );
    }

    /// An app that finishes its last activity itself (no removeTask): the same normal end, named so.
    #[test]
    fn finishing_the_last_activity_is_a_normal_end() {
        let services = format!(
            "{AB2_UP}services: process com.rovio.baba (pid 89975) died\n\
             services: last app process ended: pid 89975 finished at=1794925014.600\n"
        );
        let atld = "atld: process 89975 ended: exit status: 137 (signal 9)\n";
        let ok = ended_run(&services, AB2_PRESENT, atld).expect("normal end");
        assert!(
            ok.ends_with("ended: app finished its last activity"),
            "{ok}"
        );
    }

    /// Cut the Rope 3's quit dialog: the process exits 0 with its activity live. Exit 0 is the app's own end; any other status of a live death is not.
    #[test]
    fn an_exit_0_with_a_live_activity_is_a_normal_end() {
        let services = format!(
            "{AB2_UP}services: process com.zeptolab.ctr.ads (pid 5150) died\n\
             services: last app process ended: pid 5150 live at=1794925014.600\n"
        );
        let ok = ended_run(
            &services,
            AB2_PRESENT,
            "atld: process 5150 ended: exit status: 0\n",
        )
        .expect("exit 0 is a normal end");
        assert!(ok.ends_with("ended: app exited 0"), "{ok}");
        let err = ended_run(
            &services,
            AB2_PRESENT,
            "atld: process 5150 ended: exit status: 1\n",
        )
        .unwrap_err();
        assert!(err.contains("atld exited"), "{err}");
    }

    /// Temple Run (session 4, s4-tr): a FATAL EXCEPTION then the runtime's self-kill, with the activity live: still FAILED, naming the FATAL.
    #[test]
    fn a_crash_stays_failed() {
        let services = "services: activity resumed com.bytedance.sdk.openadsdk.activity.single.TTRewardVideoActivity at=1794924375.857\n\
                        services: activity settled at=1794924375.857\n\
                        services: process com.imangi.templerun (pid 700) died\n\
                        services: last app process ended: pid 700 live at=1794924377.600\n";
        let app = "1794924375.900 D atl-composer: ui: present n=1 time_ns=1\n\
                   1794924376.129 E AndroidRuntime: FATAL EXCEPTION: main\n\
                   1794924377.577 I liblog/Process: Sending signal. PID: 2 SIG: 9\n";
        let atld = "atld: process 700 ended: exit status: 137 (signal 9)\n";
        assert!(app_end(services, atld).is_none());
        let err = ended_run(services, app, atld).unwrap_err();
        assert!(err.starts_with("FATAL EXCEPTION at +272 ms"), "{err}");
        // a signal while the activity is live, no FATAL: still the kill
        let killed = app.replace(
            "1794924376.129 E AndroidRuntime: FATAL EXCEPTION: main\n",
            "",
        );
        assert!(
            ended_run(services, &killed, atld)
                .unwrap_err()
                .contains("the process was killed")
        );
    }
}
