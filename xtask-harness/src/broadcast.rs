// SPDX-License-Identifier: MIT
//! `xtask test broadcast`: broadcast delivery P1 in an atld run (isolated runtime tier). Two fixture APKs (xtask-harness/testdata/broadcast): `sender` is launched, `receiver` has only a manifest receiver.
//! Checks, each a line of the apps' logs (tag AtlBcast) or the services' counters:
//! - runtime: the sender's not-exported receiver gets its own package's PING exactly once (registerReceiverWithFeature + broadcastIntentWithFeature -> scheduleRegisteredReceiver);
//! - sticky return: registerReceiver(null, STICKY) returns the stored sticky with its extra (level=42);
//! - sticky replay: a receiver registered after the sticky gets it with isInitialStickyBroadcast() true;
//! - cold start: the explicit broadcast starts the receiver app headless and its Recv.onReceive runs (scheduleReceiver after attach; counter `broadcast cold start`);
//! - sticky denied: the receiver app, without BROADCAST_STICKY, gets SecurityException ("Permission Denial") from sendStickyBroadcast;
//! - exported denial: the receiver app's implicit PING is skipped for the sender's not-exported receiver (counter `Exported Denial`, and still one `runtime received`) after [`SETTLE`];
//! - protected broadcasts (BroadcastController.broadcastIntentLocked:1061-1067, the list from framework-res's manifest): the sender's BATTERY_LOW and its sticky BATTERY_CHANGED (it holds BROADCAST_STICKY)
//!   are SecurityExceptions naming the action; registerReceiver(null, BATTERY_CHANGED) returns the battery service's sticky (the host's level, app::battery_level), not the fake level 1; a receiver for only protected actions needs no export flag at targetSdk 36
//!   (:470-472), one for an unprotected action without a flag is a SecurityException (:457-460). The unprotected PING still delivers (the runtime row).
//! - ordered: two registered receivers (priority 10, 0) and the receiver app's manifest receiver pass a result on; resultTo gets the final code (ORDER); abortBroadcast skips the rest (ABORT);
//!   an implicit intent is skipped for the restricted manifest receiver (counter `Background execution not allowed`).
//! - alarm grant (the receiver app sets its own RTC_WAKEUP alarms from Recv.java): an inexact alarm's receiver starts AlarmSvc and the start is refused for the background uid (log `app is in background uid`, counter `startService: background start refused`; AlarmManagerService :2751, :2762 gives an inexact alarm no temp allowlist); an exact alarm set after that refusal grants the start (counter `temp allowlist granted`, AlarmManagerService :2765-2781), and AlarmSvc starts once.
//!
//! Failure prints the apps' and the services' log tails; the counters are printed always.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime, counter_lines, route_fifo, set_default_route};
use crate::manifest;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, d8, aapt2 (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Print both apps' whole logs.
    #[arg(long, short)]
    verbose: bool,
}

const SENDER: &str = "xtask-harness/testdata/broadcast/sender";
const RECEIVER: &str = "xtask-harness/testdata/broadcast/receiver";
/// A cold start of each app (ART without a boot image interprets framework.jar's class init), as `xtask test app`'s limit.
const LIMIT: Duration = Duration::from_secs(180);
/// How long the negative check waits after the receiver sent its PING: one oneway delivery between two attached processes takes milliseconds; 3 s on a loaded machine.
const SETTLE: Duration = Duration::from_secs(3);
const TAIL: usize = 30;

const RUNTIME: &str = "AtlBcast: runtime received com.example.atlbcast.PING";
const STICKY_RETURNED: &str = "AtlBcast: sticky returned level=42";
const STICKY_REPLAYED: &str = "AtlBcast: sticky delivered initial=true level=42";
const MANIFEST: &str = "AtlBcast: manifest received com.example.atlbcast.EXPLICIT";
const STICKY_DENIED: &str = "AtlBcast: sticky denied: Permission Denial";
const PING_SENT: &str = "AtlBcast: foreign ping sent";
/// BroadcastController.java:1063-1065's message up to the pid (the pid and uid are the run's).
const PROTECTED_DENIED: &str = "AtlBcast: protected send denied: Permission Denial: not allowed to send broadcast android.intent.action.BATTERY_LOW from pid=";
const PROTECTED_STICKY_DENIED: &str = "AtlBcast: protected sticky denied: Permission Denial: not allowed to send broadcast android.intent.action.BATTERY_CHANGED from pid=";
const BATTERY_STICKY: &str = "AtlBcast: battery sticky level=";
/// The level the sender's fake BATTERY_CHANGED carries.
const FAKE_LEVEL: &str = "AtlBcast: battery sticky level=1";
const PROTECTED_ONLY_REGISTERED: &str = "AtlBcast: protected-only register without flag: ok";
/// The sender's ordered rows (testdata sender `orderedBroadcasts`): what each receiver logs, the receiver app's manifest receiver included (its log is read too).
const ORDER_FIRST: &str = "AtlBcast: order first code=0 data=null";
const ORDER_SECOND: &str = "AtlBcast: order second code=5 data=first";
const ORDER_MANIFEST: &str = "AtlBcast: order manifest code=5 data=first";
const ORDER_RESULT: &str = "AtlBcast: order resultTo code=7 data=first";
const ABORT_FIRST: &str = "AtlBcast: abort first code=0 data=null";
const ABORT_SECOND: &str = "AtlBcast: abort second";
const ABORT_RESULT: &str = "AtlBcast: abort resultTo code=0 data=null";
/// The receiver app's manifest receiver never gets the aborted ABORT, nor the implicit IMPLICIT (its restricted uid: BroadcastSkipPolicy :229-246).
const MANIFEST_ABORT: &str = "AtlBcast: manifest received com.example.atlbcast.ABORT";
const MANIFEST_IMPLICIT: &str = "AtlBcast: manifest received com.example.atlbcast.IMPLICIT";
/// BroadcastSkipPolicy.java:244's message up to its ':' (the counter name the services count).
const RESTRICTED_SKIP: &str = "Background execution not allowed";
/// BroadcastController.java:457-460's message, callerPackage first.
const UNFLAGGED_DENIED: &str = "AtlBcast: unflagged register denied: com.example.atlbcast: One of RECEIVER_EXPORTED or RECEIVER_NOT_EXPORTED should be specified when a receiver isn't being registered exclusively for system broadcasts";
/// Counter names the services count (activity.rs): the cold start of a manifest receiver's process, and the skip of an unexported registered receiver.
const COLD_START: &str = "broadcast cold start";
const EXPORTED_DENIAL: &str = "Exported Denial";
/// The headless grace this gate sets (atld `--empty-process-grace-ms`, a short one so the row ends inside the run): the explicit broadcast's receiver process ends this long after its
/// receiver finished, once nothing holds it (activity.rs `end_headless`, plan "Linux conventions"); the default is AOSP's settle period (60 s).
const EMPTY_GRACE_MS: u64 = 2000;
/// The alarm chain's log lines (Recv.java): the refused start of the inexact alarm's receiver (ContextImpl :2049 `Not allowed to start service`, ActiveServices :1112 `app is in background uid`),
/// AlarmSvc's start (once: the exact alarm's), and the counters the refusal (active_services.rs start_service_locked) and the grant (activity.rs grant_temp_locked) count.
const ALARM_REFUSED: &str = "AtlBcast: control start refused: Not allowed to start service";
const ALARM_BACKGROUND: &str = "app is in background uid ";
const ALARM_SERVICE_STARTED: &str = "AtlBcast: alarm service started";
const REFUSED_COUNTER: &str = "startService: background start refused";
const GRANTED_COUNTER: &str = "temp allowlist granted (tempAllowlistUidLocked)";

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn count(text: &str, marker: &str) -> usize {
    text.lines().filter(|l| l.contains(marker)).count()
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let _ = Layout::of(root)?;
    let mut rt = Runtime::new(
        root,
        "broadcast",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "broadcast",
        args.verbose,
    )?
    .with_empty_process_grace(EMPTY_GRACE_MS);
    let work = rt.out.join("broadcast");
    let mut packages = Vec::new();
    let mut names = Vec::new();
    for (fixture, dir) in [(SENDER, "sender"), (RECEIVER, "receiver")] {
        let apk = rt
            .apk
            .build(&root.join(fixture), &work.join(dir), &[], None, None)?;
        let m = manifest::read(&rt.apk.aapt2, &apk)?;
        println!(
            "broadcast: built {} ({}, {} receivers)",
            apk.display(),
            m.name,
            m.receivers.len()
        );
        packages.push(Package::base(apk));
        names.push(m.name);
    }
    let (sender, receiver) = (names[0].clone(), names[1].clone());
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(sender.clone()),
        packages,
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    let mut atld = rt.start_atld(&run)?;
    let read = atl_diag::logfiles::read;
    let (sender_log, receiver_log) = (run.log(&sender), run.log(&receiver));
    let start = Instant::now();
    let mut ping_seen: Option<Instant> = None;
    // the headless end follows the receiver's finish by EMPTY_GRACE_MS, which under a loaded host can be later than SETTLE after PING: wait for its line (LIMIT caps the wait)
    let end_line = format!("process {receiver} ended: empty headless (grace {EMPTY_GRACE_MS} ms)");
    let mut end_seen = false;
    // the alarm chain's last step (Recv.java ALARM_GRANT): the run ends only once AlarmSvc started, so the grant is inside the run
    let mut alarm_seen = false;
    let ended = loop {
        if ping_seen.is_none() && read(&receiver_log).contains(PING_SENT) {
            ping_seen = Some(Instant::now());
        }
        if ping_seen.is_some() && !end_seen {
            end_seen = read(&run.log("services")).contains(&end_line);
        }
        if !alarm_seen {
            alarm_seen = read(&receiver_log).contains(ALARM_SERVICE_STARTED);
        }
        if ping_seen.is_some_and(|t| t.elapsed() > SETTLE && end_seen && alarm_seen)
            || start.elapsed() > LIMIT
        {
            break None;
        }
        if let Some(s) = atld.try_wait()? {
            break Some(s);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if ended.is_none() {
        let _ = atld.kill();
        atld.wait()?;
    }
    let (s, r) = (read(&sender_log), read(&receiver_log));
    let services = read(&run.log("services"));
    if args.verbose {
        println!("---- sender log\n{s}\n---- receiver log\n{r}");
    }
    let counters = counter_lines(&services);
    for l in &counters {
        println!("broadcast: counter {l}");
    }
    let counted = |name: &str| counters.iter().any(|l| l.contains(name));
    let system_level = format!("{BATTERY_STICKY}{}", crate::app::battery_level());
    let mut checks = vec![
        (
            "runtime receiver got its package's broadcast once",
            count(&s, RUNTIME) == 1,
        ),
        (
            "registerReceiver(null) returned the sticky",
            s.contains(STICKY_RETURNED),
        ),
        (
            "a new receiver got the sticky as initial",
            s.contains(STICKY_REPLAYED),
        ),
        (
            "explicit broadcast cold-started the receiver app",
            r.contains(MANIFEST) && counted(COLD_START),
        ),
        (
            "sticky send without BROADCAST_STICKY threw SecurityException",
            r.contains(STICKY_DENIED),
        ),
        (
            "foreign PING skipped for the not-exported receiver",
            ping_seen.is_some() && counted(EXPORTED_DENIAL) && count(&s, RUNTIME) == 1,
        ),
        (
            "an app's protected BATTERY_LOW threw SecurityException",
            s.contains(PROTECTED_DENIED),
        ),
        (
            "an app's protected sticky BATTERY_CHANGED threw SecurityException",
            s.contains(PROTECTED_STICKY_DENIED),
        ),
        (
            "registerReceiver(null, BATTERY_CHANGED) returns the system's sticky, not the app's fake",
            s.lines().any(|l| l.ends_with(&system_level))
                && !s.lines().any(|l| l.ends_with(FAKE_LEVEL)),
        ),
        (
            "a receiver of only protected actions registered without an export flag",
            s.contains(PROTECTED_ONLY_REGISTERED),
        ),
        (
            "a receiver of an unprotected action without an export flag threw SecurityException",
            s.contains(UNFLAGGED_DENIED),
        ),
        (
            "an ordered broadcast passed the result on through two registered receivers and the receiver app's manifest receiver, and resultTo got the final code",
            s.contains(ORDER_FIRST)
                && s.contains(ORDER_SECOND)
                && r.contains(ORDER_MANIFEST)
                && s.contains(ORDER_RESULT),
        ),
        (
            "abortBroadcast skipped the later receivers (manifest included) and resultTo still got the result",
            s.contains(ABORT_FIRST)
                && !s.contains(ABORT_SECOND)
                && !r.contains(MANIFEST_ABORT)
                && s.contains(ABORT_RESULT),
        ),
        (
            "an implicit broadcast was skipped for the receiver app's restricted manifest receiver (counter)",
            !r.contains(MANIFEST_IMPLICIT) && counted(RESTRICTED_SKIP),
        ),
        (
            "the explicit broadcast's receiver process ended once its receiver finished and the grace ran",
            services.contains(&end_line),
        ),
        (
            "the inexact alarm's receiver start was refused for the background uid, and counted",
            r.lines()
                .any(|l| l.contains(ALARM_REFUSED) && l.contains(ALARM_BACKGROUND))
                && counted(REFUSED_COUNTER),
        ),
        (
            "the exact alarm granted the receiver's uid a temp allowlist, and AlarmSvc started once (the grant's start)",
            count(&r, ALARM_SERVICE_STARTED) == 1 && counted(GRANTED_COUNTER),
        ),
    ];
    checks.extend(connectivity_checks(root, &mut rt, &work)?);
    let mut whys = Vec::new();
    let total = checks.len();
    for (what, ok) in checks {
        println!("broadcast: {} {what}", if ok { "pass" } else { "FAIL" });
        if !ok {
            whys.push(what.to_owned());
        }
    }
    if whys.is_empty() {
        println!(
            "broadcast: pass ({} checks in {:.1?})",
            total,
            start.elapsed()
        );
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(e) = ended {
        whys.push(format!("atld exited {e}"));
    }
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- sender log (last {TAIL})\n{}", tail(&s, TAIL));
    println!("---- receiver log (last {TAIL})\n{}", tail(&r, TAIL));
    println!("---- services log (last {TAIL})\n{}", tail(&services, TAIL));
    println!("{}", crate::verdict::gate("broadcast", &whys));
    Ok(ExitCode::FAILURE)
}

/// The connectivity row (NetworkCallback + CONNECTIVITY_ACTION sticky): atld runs in a private network namespace with a default route; the gate removes the route and restores
/// it with `set_default_route` (nsenter into atld's namespaces). Each step waits for the app's log line, the callbacks' sticky reads come 500 ms after each callback.
const NET_APP: &str = "xtask-harness/testdata/connectivity/app";
const NET_START: &str = "AtlNet: available 1";
const NET_LOST: &str = "AtlNet: lost";
const NET_AGAIN: &str = "AtlNet: available 2";
const NET_STICKY_START: &str = "AtlNet: sticky after available 1 present noConnectivity=false";
const NET_STICKY_LOST: &str = "AtlNet: sticky after lost present noConnectivity=true";
const NET_STICKY_AGAIN: &str = "AtlNet: sticky after available 2 present noConnectivity=false";
/// Each step's limit: a route change to its callback under a loaded host.
const NET_STEP: Duration = Duration::from_secs(60);

fn connectivity_checks(
    root: &Path,
    rt: &mut Runtime,
    work: &Path,
) -> Result<Vec<(&'static str, bool)>> {
    let dir = work.join("connectivity");
    let apk = rt
        .apk
        .build(&root.join(NET_APP), &dir.join("app"), &[], None, None)?;
    let package = manifest::read(&rt.apk.aapt2, &apk)?.name;
    let run = Run {
        root: dir.join("root"),
        scratch: dir.join("scratch"),
        launch: Some(package.clone()),
        packages: vec![Package::base(apk)],
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    rt.set_routed(true);
    let started = rt.start_atld(&run);
    rt.set_routed(false);
    let mut atld = started?;
    let (fifo, app) = (route_fifo(&run), run.log(&package));
    let read = atl_diag::logfiles::read;
    // waits up to NET_STEP for the app's log to hold `marker`
    let seen = |marker: &str| -> bool {
        let start = Instant::now();
        while start.elapsed() < NET_STEP {
            if count(&read(&app), marker) >= 1 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    };
    // each step needs the one before it: a failed step leaves the later ones unchecked (false)
    let start_seen = seen(NET_START);
    let sticky_start = start_seen && seen(NET_STICKY_START);
    let lost_seen = start_seen && set_default_route(&fifo, false).is_ok() && seen(NET_LOST);
    let sticky_lost = lost_seen && seen(NET_STICKY_LOST);
    let again_seen = lost_seen && set_default_route(&fifo, true).is_ok() && seen(NET_AGAIN);
    let sticky_again = again_seen && seen(NET_STICKY_AGAIN);
    let _ = atld.kill();
    atld.wait()?;
    let checks = vec![
        (
            "NetworkCallback got onAvailable at start, and the sticky was connected",
            start_seen && sticky_start,
        ),
        (
            "removing the default route in atld's namespace gave the callback onLost",
            lost_seen,
        ),
        (
            "the sticky after the loss said noConnectivity (sendLegacyNetworkBroadcast DISCONNECTED)",
            sticky_lost,
        ),
        (
            "restoring the route gave onAvailable again (a new netId), and the sticky was connected",
            again_seen && sticky_again,
        ),
    ];
    if checks.iter().any(|(_, ok)| !ok) {
        println!(
            "---- connectivity app log (last {TAIL})\n{}",
            tail(&read(&app), TAIL)
        );
        println!(
            "---- connectivity services log (last {TAIL})\n{}",
            tail(&read(&run.log("services")), TAIL)
        );
    }
    Ok(checks)
}
