// SPDX-License-Identifier: MIT
//! `xtask run-apk <ROW|PATH>`: a test APK row of test-apks.toml, or an APK file given by path (unpinned), started by atld the way an app is (its launcher activity), in UI mode: the app's window is a GTK window of this desktop with --hold, else of the gate's headless compositor (nested_wayland.rs).
//! Staging is the window gate's: the runtime (Runtime::new, gralloc, the display), the APK copied into the run's work dir, the package file from the APK's real manifest (manifest::read).
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
use atl_services_types::native_libs::{SupportedAbi, find_supported_abi};
use atl_services_types::packages::Manifest;

use crate::nested_wayland::NestedCompositor;
use crate::{arches, drive, manifest, observe, remote, test_apks, verdict, window};

/// The app process's log line for a presented frame (atl-composer ui_window.rs, debug level: UI mode enables it).
pub(crate) const PRESENT_LINE: &str = "atl-composer: ui: present";
/// The staged runtime's name, shared with the window gate: one ART install for both.
pub(crate) const STAGE: &str = "window-art";
/// What a remote run returns under its scratch dir besides the logs: atld's log, its exit code (`atld.rc`) and its end time (`end.ms`, the target's clock).
const END_FILES: [&str; 3] = ["atld.log", "atld.rc", "end.ms"];
/// How long past the target's limit the transfer may run (its tar and ssh) before it is stopped and the run fails.
const TRANSFER_MARGIN: Duration = Duration::from_secs(60);

#[derive(clap::Args)]
pub struct Args {
    /// A row of test-apks.toml (lowercase letters, digits, `-`): its pinned APK is downloaded and verified into `<workspace>/cache/test-apks` on first use.
    /// Any other argument is a file path (it holds `.`, `/` or `\`), relative to the current directory: that APK runs unpinned, no row checks it.
    #[arg(value_name = "ROW|PATH")]
    target: String,
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
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
    /// A variable in every app process's environment (repeat), atld `--app-env`: test-environment setup such as `LD_DEBUG=bindings` (the launcher-closure measurement; glibc's lines land in the app log). The product reads none of these.
    #[arg(long = "app-env", value_name = "KEY=VALUE")]
    app_env: Vec<String>,
}

/// `xtask remote run-apk` (remote.rs `Remote::RunApk`): the same run on a target's desktop. `--hold` is required: the window is the target's, on its Wayland session, not a headless one.
#[derive(clap::Args)]
pub struct RemoteArgs {
    /// A row of test-apks.toml, or an APK file path, as for `run-apk`.
    #[arg(value_name = "ROW|PATH")]
    target: String,
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
    /// A pointer script atld injects from the app's first present (`run-apk --pointer`).
    #[arg(long, value_name = "SCRIPT")]
    pointer: Option<String>,
    /// A variable in atld's environment on the target, `KEY=VALUE`, repeatable: what the desktop sets for a launch, e.g. NVIDIA's offload `__NV_PRIME_RENDER_OFFLOAD=1` (the run is otherwise `env -i`).
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env)]
    env: Vec<(String, String)>,
    /// The native out dir holding the record, the bundle and the run (default: this checkout's slot, see `xtask slot`).
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
    /// A `--drive` run of the APK file `apk`, for `xtask test drive`.
    pub(crate) fn driven(
        apk: &Path,
        host: &str,
        out: Option<PathBuf>,
        java_out: Option<PathBuf>,
        script: PathBuf,
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
            pointer: None,
            drive: Some(script),
            break_blank_capture,
            tilt_keys: false,
            app_env: Vec::new(),
        }
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
    let work = rt.out.join("run-apk");
    std::fs::create_dir_all(&work)?;
    // without --hold nobody watches the window: it maps in the gate's own compositor, which outlives atld (declared before it)
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
    // the display at the rate of the compositor the window maps on
    let rt = match (&nested, &proxy) {
        (Some(n), Some(p)) => rt
            .with_wayland(&p.listen)
            .with_display(n.display().to_arg()),
        (Some(n), None) => rt
            .with_wayland(&n.socket)
            .with_display(n.display().to_arg()),
        (None, _) => rt.with_display(run_display().to_arg()),
    };
    let m = package(&rt, &files, pin.as_ref(), &args.target)?;
    let run = run_of(&work, files, &m, &args.log_tags);
    let app_log = run.log(&m.name);
    let services_log = run.log("services");
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let hold_ms = args.hold.map(|s| s * MS_PER_S);
    let watch_ms = hold_ms.unwrap_or(window::WINDOW_MS);
    // a pointer script runs from the first present: the run lasts at least its waits plus the watch, and faults count from the last resume to the run's end
    let script_ms = args.pointer.as_deref().map_or(0, script_wait_ms);
    let started = Instant::now();
    let mut atld = rt.start_atld(&run)?;
    let mut ended = None;
    let mut complete = false;
    let mut driven = None;
    loop {
        let services = read(&services_log);
        let scripted = started.elapsed() >= Duration::from_millis(script_ms + watch_ms);
        let done = scripted
            && last_resumed(&services).is_some_and(|r| {
                window::now_ms() >= r.ms + watch_ms
                    && settled(&services)
                    && (hold_ms.is_some() || presented_since(&read(&app_log), r.ms) > 0)
            });
        if done {
            complete = true;
            if let (Some(steps), Some(proxy), Some(nested)) = (&steps, &proxy, &nested) {
                let target = drive::Target {
                    proxy,
                    nested,
                    app_log: &app_log,
                    present_line: PRESENT_LINE,
                    alive: &|| !run.app_ended(),
                    break_blank: args.break_blank_capture,
                };
                driven = Some(drive::run(steps, target));
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
    if ended.is_none() {
        window::stop_apps(atld.id());
        run.stop(&mut atld)?;
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
    };
    let scripted = args.pointer.is_some() || args.drive.is_some();
    let outcome = check_delivery(foreign.as_deref(), &m.name, &app)
        .and_then(|()| judge_ending(&services, &app, &ending, watch_ms, scripted));
    // the drive's failure first: it says what the script could not do, the verdict only what the app did
    let outcome = match (driven, args.drive.is_some()) {
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
    let arches = arches::load(root)?;
    let (arch, _) = arches::build_machine(root, &arches)?;
    let args = Args {
        target: a.target.clone(),
        host: arch.to_owned(),
        out: a.out.clone(),
        java_out: None,
        hold: Some(a.hold),
        log_tags: "*:i".to_owned(),
        verbose: false,
        no_app_seccomp: false,
        break_isa: false,
        pointer: a.pointer.clone(),
        drive: None,
        break_blank_capture: false,
        tilt_keys: a.tilt_keys,
        app_env: Vec::new(),
    };
    // no `--display`: atld reads the target's own monitor, as on the product's host (host_display); this host's monitor is not the target's
    let (rt, foreign) = runtime(root, &args, &files)?;
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
    let target = remote::Target::of(&rt.out, "run-apk", &a.host, a.brk)?;
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
    target.push()?;
    let prereq = target.check()?;
    let problems = prereq.problems();
    if !problems.is_empty() {
        for p in &problems {
            println!("remote prereq run-apk: {p}");
        }
        println!("{}", verdict::gate("remote run-apk", &problems));
        return Ok(ExitCode::FAILURE);
    }
    println!(
        "remote prereq run-apk: PASS, {} ELF checked on {}",
        prereq.checked,
        target.label()
    );
    // what an earlier run left here is not this run's: a run that ends before its logs return reads none of it
    let _ = std::fs::remove_dir_all(&logs);
    std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
    for name in END_FILES {
        let _ = std::fs::remove_file(run.scratch.join(name));
    }
    let rt = rt.with_site(target.run_site(limit, logs.join("transcript.log"), true));
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
        };
        check_delivery(foreign.as_deref(), &m.name, &app)?;
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

/// The app's ISA when foreign (not `host`): the device ABI the install picks over every file of the package (atl-services-types `find_supported_abi`; a config split carries the libraries), whose guest and
/// engine the test image then needs. Empty for a host-ISA or no-native-code app.
fn foreign_isa(abis: &[SupportedAbi], host: &str, files: &[PackageFile]) -> Result<Option<String>> {
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
        .map(|i| abis[i].isa.clone())
        .filter(|isa| isa != host))
}

/// The staged runtime of a run of `args` for the package `files`: the window gate's staging, with the GL driver, the native bridge guest of the host and, for a foreign-ISA app ([`foreign_isa`]), that ISA's guest and QEMU engine, and the GTK UI. Its display is set by the caller.
fn runtime(root: &Path, args: &Args, files: &[PackageFile]) -> Result<(Runtime, Option<String>)> {
    let rt = Runtime::new(
        root,
        "run-apk",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        STAGE,
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver(root)?
    .with_native_bridge_guest(&args.host)?
    .with_ui("gtk");
    let foreign = foreign_isa(rt.abis(), &args.host, files)?;
    let rt = match &foreign {
        Some(isa) => {
            crate::build_state::foreign_out(root, &rt.out, &args.host, isa, "run-apk")?;
            rt.with_native_bridge_guest(isa)?
                .with_qemu_engine(root, isa)?
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
    let rt = match &args.pointer {
        Some(script) => rt.with_headless_pointer(script),
        None => rt,
    };
    let rt = if args.tilt_keys {
        rt.with_tilt_keys(&[])
    } else {
        rt
    };
    Ok((rt.with_app_env(&args.app_env), foreign))
}

/// The bridge's load line (atl-native-bridge loader.rs) for a library of `package`'s `lib/<isa>/`.
fn delivery_line(package: &str, isa: &str) -> String {
    format!("atl-native-bridge: loaded /data/app/{package}/lib/{isa}/")
}

/// The delivery check of a foreign-ISA app (`foreign`): the app log `app` has the bridge loading one of the package's `lib/<isa>/` libraries into the guest, so ART handed the bridge the app's ISA and the engine took the library.
/// Checked before the window verdict: without delivery nothing the app's native code does is the guest's.
fn check_delivery(foreign: Option<&str>, package: &str, app: &str) -> Result<(), String> {
    let Some(isa) = foreign else {
        return Ok(());
    };
    let line = delivery_line(package, isa);
    match app.lines().find(|l| l.contains(&line)) {
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
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the APK has no launcher activity")?;
    println!(
        "run-apk: {} ({} versionCode {}, sdk {}..{}, launcher activity {activity}, {} file(s))",
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
        packages: vec![Package {
            files,
            manifest: m.clone(),
        }],
        first_uid: window::APP_UID,
        libraries: vec![],
        shell: None,
        log_tags: log_tags.to_owned(),
    }
}

/// Waits for the transfer that runs the app on the target: its status, or `None` once `limit` has passed (the transfer is then killed). `tick` runs between the status checks.
fn wait_transfer(
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
fn end_record(scratch: &Path) -> Result<(i32, u64), String> {
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
fn stopped_at_limit(rc: i32) -> bool {
    matches!(rc, 124 | 137)
}

/// Whether a remote run's watch ran to its end: the target's end came `watch_ms` after the last resume. Both times are on the target's clock.
fn watch_complete(services: &str, watch_ms: u64, end_ms: u64) -> bool {
    last_resumed(services).is_some_and(|r| end_ms >= r.ms + watch_ms)
}

/// How a run ended, as the judge reads it: the watch ran to its end, atld ended before its end by itself, and the end's time on the clock the logs carry (this host's, or the target's).
struct Ending {
    complete: bool,
    atld_exited: bool,
    end_ms: u64,
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
    judge(services, app, ending.complete, ending.atld_exited, fault_ms)
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

/// The run's printed verdict: the pass line, or the failure with the app's and the services' log tails. Then atld's end lines, and where the logs are.
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
    // the app's end as its parent saw it: a signal leaves no line of its own in the app's or the services' log
    for line in std::fs::read_to_string(atld_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(" ended: "))
    {
        println!("run-apk: {line}");
    }
    println!("run-apk: logs {}", log_dir.display());
    if outcome.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The package to run, base first: a row's pinned files (verified by `test_apks::ensure`), or the one base APK a path names, which no row pins.
fn source(root: &Path, target: &str) -> Result<(Option<test_apks::Row>, Vec<PackageFile>)> {
    if test_apks::names_row(target) {
        let (row, files) = test_apks::ensure(root, target)?;
        let files = files
            .into_iter()
            .map(|(split, path)| PackageFile { split, path })
            .collect();
        return Ok((Some(row), files));
    }
    let path = PathBuf::from(target);
    ensure!(
        path.is_file(),
        "{target}: not a file (an argument holding `.`, `/` or `\\` is a path)"
    );
    Ok((None, vec![PackageFile { split: None, path }]))
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

/// The run's verdict over its two logs, about the last activity the app resumed: the services are settled (nothing pending after that resume), no fault in the app log within
/// `watch_ms` after it (`atld_exited` is a fault at the window's end), the watch was run to its end (`complete`), and a frame was presented after it.
/// Ok: the pass line; Err: the first failed part.
fn judge(
    services: &str,
    app: &str,
    complete: bool,
    atld_exited: bool,
    watch_ms: u64,
) -> Result<String, String> {
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
    if !complete {
        return Err(format!(
            "the run ended before the {watch_ms} ms watch after {} was over",
            last.class
        ));
    }
    let frames = presented_since(app, last.ms);
    if frames == 0 {
        return Err(format!(
            "no frame presented after {} resumed: the app log has no `{PRESENT_LINE}` line after it",
            last.class
        ));
    }
    Ok(format!(
        "onResume of {} at {}, {frames} frame(s) presented after it, settled, no fault in the {watch_ms} ms after it",
        last.class,
        window::stamp_text(last.ms)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load line as loader.rs prints it for CTR's arm64 libctro (measured 2026-11-12); the break is identity's run, which loads nothing into a guest of the app's ISA.
    #[test]
    fn delivery_needs_the_guest_load_of_the_apps_isa() {
        let pkg = "com.zeptolab.ctr.ads";
        let app = "atl-native-bridge: loaded /data/app/com.zeptolab.ctr.ads/lib/arm64/libctro.so into the guest\n";
        assert!(check_delivery(Some("arm64"), pkg, app).is_ok());
        assert!(check_delivery(Some("arm64"), pkg, "").is_err());
        assert!(check_delivery(Some("riscv64"), pkg, app).is_err());
        // a host-ISA app has nothing to deliver
        assert!(check_delivery(None, pkg, "").is_ok());
    }

    /// Lines as atld prints them (atld main.rs `run`): a GPU desktop, its display tier, the trial tier, and WSLg's vgem-only devices.
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
        assert!(judge(RESUME, &app, true, false, 2_000).is_ok());
    }

    #[test]
    fn judge_fails_each_missing_or_broken_part() {
        let err = |services: &str, app: &str, complete: bool, atld_exited: bool| {
            judge(services, app, complete, atld_exited, 2_000).unwrap_err()
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
        let err = judge(services, app, true, false, 2_000).unwrap_err();
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
        let err = judge(services, PRESENT, true, false, 2_000).unwrap_err();
        assert!(err.contains("no frame presented after org.x.Main"), "{err}");
    }

    #[test]
    fn script_waits_are_summed() {
        assert_eq!(script_wait_ms("wait:6000 down:1,2 up:1,2 wait:2500"), 8500);
        assert_eq!(script_wait_ms("down:1,2 wait:x"), 0);
    }

    #[test]
    fn judge_ignores_a_fault_after_the_watch() {
        let app = format!("{PRESENT}102.500 E AndroidRuntime: FATAL EXCEPTION: main\n");
        assert!(judge(RESUME, &app, true, false, 2_000).is_ok());
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
        };
        assert!(judge_ending(RESUME, PRESENT, &ending(102_000), 2_000, false).is_ok());
        let err = judge_ending(RESUME, PRESENT, &ending(101_000), 2_000, false).unwrap_err();
        assert!(err.contains("ended before"), "{err}");
    }
}
