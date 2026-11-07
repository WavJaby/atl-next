// SPDX-License-Identifier: MIT
//! `xtask test sensors`: the sensor service gate. The fixture APK (xtask/testdata/sensors) uses SensorManager in onCreate under atld, as `test app` runs its app; atld runs in a private user + mount namespace with a fake `/sys/bus` and `/run/udev/data` the gate writes (atld_run.rs `with_fake_devices`), so the product reads its fixed IIO and udev paths.
//! Stage `empty`: no IIO device. Pass = the lookup (getSystemService + getDefaultSensor) within [`LOOKUP_LIMIT`], 0 sensors, default accelerometer null, registerListener false, no fault. Break `unregistered`: the services skip `sensorservice`, so SensorManager waits for it (60 tries, SensorManager.cpp `waitForSensorService`) and the lookup limit fails.
//! Stage `iio`: two fake accelerometers, the owner's laptop's display one (its upright reading, its hwdb mount matrix as the udev property `ACCEL_MOUNT_MATRIX`, a decoy sysfs `in_accel_mount_matrix` the property must beat, `ACCEL_LOCATION=display`) and a base one
//! (`ACCEL_LOCATION=base`). Pass = only the display one listed, as android.sensor.accelerometer, >= [`MIN_EVENTS`] events, each with the values derived below, timestamps increasing on the elapsedRealtimeNanos base, and the services' poll stopped after unregisterListener.
//! Breaks: `hwdb` (no udev entry for the display device: the decoy matrix applies), `location` (no udev entry for the base device: it is listed), `scale` (doubled scale written) -- each while the real values / count are expected; `frame` expects the canonical values (no canonical -> Android conversion). Each must fail.
//! Stage `tilt`: no IIO device, atld `--tilt-keys` with [`TILT_ARGS`], the headless script [`TILT_SCRIPT`] (focus in, hold Left, hold Right, then hold Left and lose focus). Pass = the injected accelerometer listed, every event of magnitude g with y = 0, level first,
//! the Left plateau x = +g sin(max) before the Right plateau x = -g sin(max), no |x| beyond it, level last (the focus loss), the services' injection-on line. Breaks: `off` (no `--tilt-keys`: nothing listed), `real` (the iio stage's devices present: the services refuse injection and list the real one). Each must fail.
//! Expected values, computed here independently: axis = raw x scale (offset 0, kernel sysfs-bus-iio), m/s²; canonical r = mount-matrix row r . sensor vector (iio-sensor-proxy `apply_mount_matrix`); Android = (-x, -y, z) of canonical (iio.rs `to_android` derives it).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::atld_run::{Run, Runtime, counter_lines};
use crate::window::{self, WINDOW_MS};
use crate::{manifest, verdict};

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Stage {
    Empty,
    Iio,
    Tilt,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Break {
    /// stage empty: `sensorservice` not registered
    Unregistered,
    /// stage iio: no udev entry for the display device (the decoy sysfs matrix applies), the hwdb matrix's values expected
    Hwdb,
    /// stage iio: no udev entry for the base device (it is listed), one sensor expected
    Location,
    /// stage iio: canonical (unconverted) values expected
    Frame,
    /// stage iio: doubled scale written, real values expected
    Scale,
    /// stage tilt: atld runs without `--tilt-keys`
    Off,
    /// stage tilt: a real accelerometer present (the iio stage's devices)
    Real,
}

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    #[arg(long, value_enum)]
    stage: Stage,
    /// A deliberate break: the gate must then fail.
    #[arg(long = "break", value_enum)]
    break_: Option<Break>,
    /// Native build directory (default: this checkout's slot).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory (default: this checkout's slot).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask/testdata/sensors";
/// The fixture's log tag, as the launcher's logger writes it.
const TAG: &str = "AtlSensors: ";
/// What onCreate logs last.
const DONE: &str = "AtlSensors: done";
const APP_UID: u32 = 10_124;
/// A cold start (app.rs LIMIT).
const LIMIT: Duration = Duration::from_secs(180);
/// The lookup with the service present takes one binder round trip per call; without it SensorManager waits 60 x getService (>= 60 s). 10 s separates the two on a loaded machine.
const LOOKUP_LIMIT_MS: u64 = 10_000;
/// How long the gate waits for `done` after the lookup line (the fixture's own wait is 3 s).
const AFTER_LOOKUP: Duration = Duration::from_secs(30);
/// The fixture logs 5 events (Main.EVENTS); fewer within its 3 s wait fails.
const MIN_EVENTS: usize = 5;
/// float32 rounding of the Java log's decimal form.
const TOLERANCE: f64 = 1e-4;
/// How long after `done` the services' poll must have logged its stop (the poll sleeps one period, 20 ms at GAME).
const STOP_WAIT: Duration = Duration::from_secs(5);
const TAIL: usize = 40;

/// The display device's reading: the owner's laptop upright, measured 2026-11-06 as (-0.06, -8.71, -3.74) m/s², here in steps of [`SCALE`]. Every axis is non-zero, so each axis's sign is checked.
const RAW: [i64; 3] = [-6, -871, -374];
/// One raw step, m/s²: the hundredths the measurement was read in.
const SCALE: f64 = 0.01;
/// The laptop's hwdb `ACCEL_MOUNT_MATRIX` (owner, 2026-11-06): z negated.
const MATRIX: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]];
/// A sysfs `in_accel_mount_matrix` the udev property must beat (an x/y swap: its values differ on every axis pair).
const DECOY: [[f64; 3]; 3] = [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
const DEVICE_NAME: &str = "atl-fake-accel";
const BASE_NAME: &str = "atl-fake-base-accel";
/// The fake devices' char device numbers (sysfs `dev`), which key their udev entries.
const DEVICE_DEV: &str = "250:0";
const BASE_DEV: &str = "250:1";

/// atl-services sensor.rs INJECTED_NAME.
const INJECTED_NAME: &str = "ATL injected accelerometer";
/// The tilt the gate sets: 30 deg (x = g/2), a 300 ms ramp (a fifth of a hold), 20 ms period (SENSOR_DELAY_GAME).
const TILT_MAX_DEG: f64 = 30.0;
const TILT_ARGS: [&str; 6] = [
    "--tilt-max-deg",
    "30",
    "--tilt-ramp-ms",
    "300",
    "--tilt-period-ms",
    "20",
];
/// Linux KEY_LEFT / KEY_RIGHT (input-event-codes.h). Holds of 1.5 s: five ramps, so each plateau gets ~60 events; the last hold ends in a focus loss, which must level the device.
const TILT_SCRIPT: &str = "focus:in wait:1500 key-down:105 wait:1500 key-up:105 wait:1000 key-down:106 wait:1500 key-up:106 wait:1000 key-down:105 wait:1500 focus:out wait:1000";
/// SensorManager.GRAVITY_EARTH.
const GRAVITY: f64 = 9.806_65;
/// float32 trig and the Java log's decimal form.
const TILT_TOLERANCE: f64 = 1e-3;

/// The stage tilt's checks over the event values in order; empty = pass.
fn check_tilt(values: &[Vec<f64>]) -> Vec<String> {
    let mut whys = Vec::new();
    let plateau = GRAVITY * TILT_MAX_DEG.to_radians().sin();
    let near = |a: f64, b: f64| (a - b).abs() <= TILT_TOLERANCE;
    let level = |v: &Vec<f64>| near(v[0], 0.0) && near(v[2], GRAVITY);
    if values.iter().any(|v| v.len() != 3) {
        whys.push("an event without 3 values".to_owned());
        return whys;
    }
    for v in values {
        let m = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if !near(m, GRAVITY) || !near(v[1], 0.0) || v[0].abs() > plateau + TILT_TOLERANCE {
            whys.push(format!(
                "event {v:?}: want |v| = g, y = 0, |x| <= g sin {TILT_MAX_DEG}"
            ));
            break;
        }
    }
    match (values.first(), values.last()) {
        (Some(f), Some(l)) => {
            if !level(f) {
                whys.push(format!("first event {f:?} not level"));
            }
            if !level(l) {
                whys.push(format!(
                    "last event {l:?} not level: the focus loss did not reset the tilt"
                ));
            }
        }
        _ => whys.push("no events".to_owned()),
    }
    let left = values.iter().position(|v| near(v[0], plateau));
    let right = values.iter().position(|v| near(v[0], -plateau));
    match (left, right) {
        (Some(l), Some(r)) if l < r => {}
        _ => whys.push(format!(
            "want x = +{plateau:.4} (Left held) before x = -{plateau:.4} (Right held): first at {left:?}, {right:?}"
        )),
    }
    whys
}

fn matrix_text(m: &[[f64; 3]; 3]) -> String {
    m.iter()
        .map(|r| {
            r.iter()
                .map(|v| format!("{v}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// RAW / SCALE through MATRIX: the Linux canonical frame.
fn canonical() -> [f64; 3] {
    let s: Vec<f64> = RAW.iter().map(|r| *r as f64 * SCALE).collect();
    let mut d = [0.0; 3];
    for (r, row) in MATRIX.iter().enumerate() {
        d[r] = row[0] * s[0] + row[1] * s[1] + row[2] * s[2];
    }
    d
}

/// The values an app must see: canonical with x and y negated.
fn expected() -> [f64; 3] {
    let c = canonical();
    [-c[0], -c[1], c[2]]
}

/// The fake `/sys/bus` and udev database (`/run/udev/data`): in stage iio `iio/devices/iio:device0` (display) and `iio:device1` (base) with their `c<dev>` entries; nothing in stage empty.
fn write_devices(sys_bus: &Path, udev: &Path, stage: Stage, brk: Option<Break>) -> Result<()> {
    for dir in [sys_bus, udev] {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
    }
    if stage == Stage::Empty || (stage == Stage::Tilt && brk != Some(Break::Real)) {
        return Ok(());
    }
    let scale = if brk == Some(Break::Scale) {
        2.0 * SCALE
    } else {
        SCALE
    };
    let display_db = format!(
        "E:ACCEL_MOUNT_MATRIX={}\nE:ACCEL_LOCATION=display\n",
        matrix_text(&MATRIX)
    );
    let devices = [
        (
            "iio:device0",
            DEVICE_NAME,
            DEVICE_DEV,
            display_db,
            brk == Some(Break::Hwdb),
        ),
        (
            "iio:device1",
            BASE_NAME,
            BASE_DEV,
            "E:ACCEL_LOCATION=base\n".to_owned(),
            brk == Some(Break::Location),
        ),
    ];
    for (id, name, dev_no, db, skip_db) in devices {
        let dev = sys_bus.join("iio/devices").join(id);
        std::fs::create_dir_all(&dev)?;
        let files = [
            ("name", name.to_owned()),
            ("dev", dev_no.to_owned()),
            ("in_accel_x_raw", RAW[0].to_string()),
            ("in_accel_y_raw", RAW[1].to_string()),
            ("in_accel_z_raw", RAW[2].to_string()),
            ("in_accel_scale", format!("{scale}")),
            ("in_accel_mount_matrix", matrix_text(&DECOY)),
        ];
        for (n, t) in files {
            std::fs::write(dev.join(n), format!("{t}\n"))
                .with_context(|| format!("writing {id}/{n}"))?;
        }
        if !skip_db {
            std::fs::write(udev.join(format!("c{dev_no}")), db)
                .with_context(|| format!("writing the udev entry of {id}"))?;
        }
    }
    Ok(())
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The fixture's messages, in order.
fn messages(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|l| l.split_once(TAG).map(|(_, m)| m.trim().to_owned()))
        .collect()
}

fn field<'a>(msg: &'a str, key: &str) -> Option<&'a str> {
    msg.split(' ')
        .find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// The stage's checks over the fixture's messages and the services log; empty = pass.
fn check(stage: Stage, msgs: &[String], services: &str, want: [f64; 3]) -> Vec<String> {
    let mut whys = Vec::new();
    let find = |p: &str| msgs.iter().find(|m| m.starts_with(p));
    match find("lookup ")
        .and_then(|m| field(m, "ms"))
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(ms) if ms <= LOOKUP_LIMIT_MS => {}
        Some(ms) => whys.push(format!(
            "lookup took {ms} ms (> {LOOKUP_LIMIT_MS}): SensorManager waited for sensorservice"
        )),
        None => whys.push("no lookup line: SensorManager stalled or the app died".to_owned()),
    }
    let n = find("list ")
        .and_then(|m| field(m, "n"))
        .and_then(|v| v.parse::<usize>().ok());
    match stage {
        Stage::Empty => {
            if n != Some(0) {
                whys.push(format!("sensor count {n:?}, want 0"));
            }
            if find("default null register=false").is_none() {
                whys.push(format!(
                    "want `default null register=false`, got {:?}",
                    find("default")
                ));
            }
        }
        Stage::Iio => {
            if n != Some(1) {
                whys.push(format!("sensor count {n:?}, want 1"));
            }
            let d = find("default ");
            let default = format!(
                "default name={DEVICE_NAME} type=1 stringType=android.sensor.accelerometer"
            );
            if !d.is_some_and(|d| d.starts_with(&default)) {
                whys.push(format!("want `{default} ...`, got {d:?}"));
            }
            if find("register=true").is_none() {
                whys.push("registerListener did not return true".to_owned());
            }
            let events: Vec<&String> = msgs.iter().filter(|m| m.starts_with("event ")).collect();
            if events.len() < MIN_EVENTS {
                whys.push(format!("{} events, want >= {MIN_EVENTS}", events.len()));
            }
            let mut last_ts = i64::MIN;
            for e in &events {
                let ts = field(e, "ts").and_then(|v| v.parse::<i64>().ok());
                let now = field(e, "now").and_then(|v| v.parse::<i64>().ok());
                let v: Option<Vec<f64>> =
                    field(e, "v").map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect());
                let (Some(ts), Some(now), Some(v)) = (ts, now, v) else {
                    whys.push(format!("unparsable event line {e:?}"));
                    continue;
                };
                if ts <= last_ts {
                    whys.push(format!("timestamp {ts} not after {last_ts}"));
                }
                last_ts = ts;
                // delivered within a second of its stamp, on the same clock
                if !(0..1_000_000_000).contains(&(now - ts)) {
                    whys.push(format!(
                        "timestamp {ts} not on the elapsedRealtimeNanos base (now {now})"
                    ));
                }
                if v.len() != 3 || v.iter().zip(want).any(|(g, w)| (g - w).abs() > TOLERANCE) {
                    whys.push(format!(
                        "values {v:?}, want {want:?} (raw x scale through the mount matrix, then to the Android frame)"
                    ));
                }
            }
            if !services
                .lines()
                .any(|l| l.contains(&format!("sensor: {DEVICE_NAME}")) && l.ends_with("stopped"))
            {
                whys.push("the services' poll did not stop after unregisterListener".to_owned());
            }
        }
        Stage::Tilt => {
            if n != Some(1) {
                whys.push(format!("sensor count {n:?}, want 1"));
            }
            let d = find("default ");
            let default = format!(
                "default name={INJECTED_NAME} type=1 stringType=android.sensor.accelerometer"
            );
            if !d.is_some_and(|d| d.starts_with(&default)) {
                whys.push(format!("want `{default} ...`, got {d:?}"));
            }
            if !services.lines().any(|l| l.contains("sensor: injection on")) {
                whys.push("no `sensor: injection on` line from the services".to_owned());
            }
            let values: Vec<Vec<f64>> = msgs
                .iter()
                .filter(|m| m.starts_with("event "))
                .filter_map(|e| field(e, "v"))
                .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect())
                .collect();
            whys.extend(check_tilt(&values));
            if !services
                .lines()
                .any(|l| l.contains(&format!("sensor: {INJECTED_NAME}")) && l.ends_with("stopped"))
            {
                whys.push(
                    "the injected sensor was not stopped after unregisterListener".to_owned(),
                );
            }
        }
    }
    if find("done").is_none() {
        whys.push("no done line".to_owned());
    }
    whys
}

/// Native goals (gate_goals.rs): the runtime with gralloc.
pub fn goals() -> crate::gate_goals::Goals {
    crate::atld_run::goals().with(crate::atld_run::gralloc_goals())
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let name = match args.stage {
        Stage::Empty => "sensors-empty",
        Stage::Iio => "sensors-iio",
        Stage::Tilt => "sensors-tilt",
    };
    let mut rt = Runtime::new(
        root,
        "sensors",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "sensors-art",
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver()?;
    let work = rt.out.join("sensors");
    let sys_bus = work.join("sys-bus");
    let udev = work.join("udev-data");
    write_devices(&sys_bus, &udev, args.stage, args.break_)?;
    rt = rt.with_fake_devices(sys_bus.clone(), udev);
    if args.break_ == Some(Break::Unregistered) {
        rt = rt.without_sensor_service();
    }
    if args.stage == Stage::Tilt {
        rt = rt.with_headless_pointer(TILT_SCRIPT);
        if args.break_ != Some(Break::Off) {
            rt = rt.with_tilt_keys(&TILT_ARGS.map(String::from));
        }
    }
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("apk"), &[], None, None)?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![(apk, m.clone())],
        first_uid: APP_UID,
        libraries: vec![],
        shell: None,
        log_tags: "*:i".to_owned(),
    };
    let mut child = rt.start_atld(&run)?;
    let app_log = run.log(&m.name);
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let start = Instant::now();
    let mut started_at: Option<Instant> = None;
    let mut lookup_at: Option<Instant> = None;
    let mut ended = None;
    // until done; a start line without a lookup line within the limit is the stall (no need to sit out SensorManager's wait)
    loop {
        let msgs = messages(&read(&app_log));
        if msgs.iter().any(|m| m == "done" || m.starts_with("done ")) {
            break;
        }
        if started_at.is_none() && msgs.iter().any(|m| m == "start") {
            started_at = Some(Instant::now());
        }
        if lookup_at.is_none() && msgs.iter().any(|m| m.starts_with("lookup ")) {
            lookup_at = Some(Instant::now());
        }
        if let Some(t) = lookup_at
            && t.elapsed() > AFTER_LOOKUP
        {
            break;
        }
        if let Some(t) = started_at
            && lookup_at.is_none()
            && t.elapsed() > Duration::from_millis(LOOKUP_LIMIT_MS)
        {
            break;
        }
        if let Some(s) = child.try_wait()? {
            ended = Some(s);
            break;
        }
        if run.app_ended() {
            ended = Some(run.stop(&mut child)?);
            break;
        }
        if start.elapsed() > LIMIT {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // the poll's stop line follows unregisterListener
    let services_path = run.log("services");
    let stop_deadline = Instant::now() + STOP_WAIT;
    while args.stage != Stage::Empty
        && Instant::now() < stop_deadline
        && !read(&services_path).lines().any(|l| l.ends_with("stopped"))
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    let app = read(&app_log);
    let fault = fault(&app, ended.is_some());
    if ended.is_none() {
        run.stop(&mut child)?;
    }
    let services = read(&services_path);
    if args.verbose {
        println!("---- app log\n{app}");
    }
    for l in counter_lines(&services) {
        println!("{name}: counter {l}");
    }
    let msgs = messages(&app);
    let want = if args.break_ == Some(Break::Frame) {
        canonical()
    } else {
        expected()
    };
    let mut whys = check(args.stage, &msgs, &services, want);
    if let Some(f) = fault {
        whys.push(format!("the app faulted: {f}"));
    }
    if let Some(s) = ended {
        whys.push(format!("atld or the app ended early: {s}"));
    }
    if whys.is_empty() {
        for m in msgs
            .iter()
            .filter(|m| !m.starts_with("event "))
            .chain(msgs.iter().find(|m| m.starts_with("event ")))
        {
            println!("{name}: {m}");
        }
        println!("{name}: pass");
        return Ok(ExitCode::SUCCESS);
    }
    for w in &whys {
        println!("{name}: FAIL {w}");
    }
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    println!(
        "---- services log (last {TAIL} lines)\n{}",
        tail(&services, TAIL)
    );
    println!("{}", verdict::gate(name, &whys));
    Ok(ExitCode::FAILURE)
}

/// The app's first fault up to `done` + WINDOW_MS (app.rs `fault`).
fn fault(app_log: &str, atld_exited: bool) -> Option<String> {
    let marker_ms = window::verdict(app_log, DONE, atld_exited, WINDOW_MS).marker_ms?;
    window::fault_after(app_log, 0, marker_ms + WINDOW_MS, marker_ms, atld_exited)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_values_follow_the_iio_abi() {
        // by hand: sensor = (-0.06, -8.71, -3.74); canonical (z negated) = (-0.06, -8.71, 3.74); Android = (0.06, 8.71, 3.74): upright y = +g, lid leaning back z > 0
        let e = expected();
        assert!((e[0] - 0.06).abs() < 1e-9);
        assert!((e[1] - 8.71).abs() < 1e-9);
        assert!((e[2] - 3.74).abs() < 1e-9);
    }

    #[test]
    fn stall_and_wrong_values_fail() {
        let empty_ok: Vec<String> = [
            "lookup ms=12",
            "list n=0",
            "default null register=false",
            "done",
        ]
        .map(String::from)
        .to_vec();
        let want = expected();
        assert!(check(Stage::Empty, &empty_ok, "", want).is_empty());
        let stalled: Vec<String> = vec![
            "lookup ms=60123".into(),
            "list n=0".into(),
            "default null register=false".into(),
            "done".into(),
        ];
        assert!(!check(Stage::Empty, &stalled, "", want).is_empty());
        let ev = |ts: i64, v: &str| format!("event ts={ts} now={} v={v}", ts + 1000);
        let mut iio: Vec<String> = vec![
            "lookup ms=5".into(),
            "list n=1".into(),
            format!(
                "default name={DEVICE_NAME} type=1 stringType=android.sensor.accelerometer minDelay=10000"
            ),
            "register=true".into(),
        ];
        for i in 0..5 {
            iio.push(ev(100 + i, "0.06,8.71,3.74"));
        }
        iio.push("done events=5".into());
        let stopped = format!("sensor: {DEVICE_NAME} (handle 1) stopped");
        assert_eq!(
            check(Stage::Iio, &iio, &stopped, want),
            Vec::<String>::new()
        );
        // the canonical (unconverted) values
        iio[4] = ev(100, "-0.06,-8.71,3.74");
        assert!(!check(Stage::Iio, &iio, &stopped, want).is_empty());
    }

    #[test]
    fn tilt_wants_level_left_then_right_then_level() {
        // by hand: g sin 30 = 4.903325, g cos 30 = 8.492829
        let lvl = vec![0.0, 0.0, GRAVITY];
        let l = vec![4.903_325, 0.0, 8.492_829];
        let r = vec![-4.903_325, 0.0, 8.492_829];
        assert!(
            check_tilt(&[lvl.clone(), l.clone(), lvl.clone(), r.clone(), lvl.clone()]).is_empty()
        );
        assert!(
            !check_tilt(&[lvl.clone(), r.clone(), l.clone(), lvl.clone()]).is_empty(),
            "order"
        );
        assert!(
            !check_tilt(&[lvl.clone(), l.clone(), r.clone(), l.clone()]).is_empty(),
            "last not level"
        );
        assert!(
            !check_tilt(&[lvl.clone(), vec![6.0, 0.0, 7.6], r, lvl]).is_empty(),
            "beyond max"
        );
    }
}
