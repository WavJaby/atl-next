// SPDX-License-Identifier: MIT
//! `xtask test broadcast`: broadcast delivery P1 in an atld run (isolated runtime tier). Two fixture APKs (xtask-harness/testdata/broadcast): `sender` is launched, `receiver` has only a manifest receiver.
//! Checks, each a line of the apps' logs (tag AtlBcast) or the services' counters:
//! - runtime: the sender's not-exported receiver gets its own package's PING exactly once (registerReceiverWithFeature + broadcastIntentWithFeature -> scheduleRegisteredReceiver);
//! - sticky return: registerReceiver(null, STICKY) returns the stored sticky with its extra (level=42);
//! - sticky replay: a receiver registered after the sticky gets it with isInitialStickyBroadcast() true;
//! - cold start: the explicit broadcast starts the receiver app headless and its Recv.onReceive runs (scheduleReceiver after attach; counter `broadcast cold start`);
//! - sticky denied: the receiver app, without BROADCAST_STICKY, gets SecurityException ("Permission Denial") from sendStickyBroadcast;
//! - exported denial: the receiver app's implicit PING is skipped for the sender's not-exported receiver (counter `Exported Denial`, and still one `runtime received`), judged once the sender logged DONE (the fence: the PING's looper);
//! - protected broadcasts (BroadcastController.broadcastIntentLocked:1061-1067, the list from framework-res's manifest): the sender's BATTERY_LOW and its sticky BATTERY_CHANGED (it holds BROADCAST_STICKY)
//!   are SecurityExceptions naming the action; registerReceiver(null, BATTERY_CHANGED) returns the battery service's sticky (the host's level, app::battery_level), not the fake level 1; a receiver for only protected actions needs no export flag at targetSdk 36
//!   (:470-472), one for an unprotected action without a flag is a SecurityException (:457-460). The unprotected PING still delivers (the runtime row).
//! - ordered: two registered receivers (priority 10, 0) and the receiver app's manifest receiver pass a result on; resultTo gets the final code (ORDER); abortBroadcast skips the rest (ABORT);
//!   an implicit intent is skipped for the restricted manifest receiver (counter `Background execution not allowed`).
//! - alarm grant (the receiver app sets its own RTC_WAKEUP alarms from Recv.java): an inexact alarm's receiver starts AlarmSvc and the start is refused for the background uid (log `app is in background uid`, counter `startService: background start refused`; AlarmManagerService :2751, :2762 gives an inexact alarm no temp allowlist); an exact alarm set after that refusal grants the start (counter `temp allowlist granted`, AlarmManagerService :2765-2781), and AlarmSvc starts once.
//!
//! - boot (plan S9, decision D2): LOCKED_BOOT_COMPLETED, then BOOT_COMPLETED through its resultTo, reach the opted-in receiver app (`--autostart`) headless; BOOT's receiver starts its service inside the 20 s
//!   temp allowlist; the sender, not opted in, gets neither at start (counter `boot broadcast skipped`), and BOOT_COMPLETED at its first process start, ahead of its activity (D2 clause 2).
//!   The receiver's boot receiver exists in its version 2 only, so the first run's restricted-start rows see no boot grant.
//! - reinstall (plan S9): a second atld run over the same data root installs version 2 of the receiver: the services classify it `Updated { from: 1 }` and MY_PACKAGE_REPLACED reaches it alone;
//!   the sender, unchanged, gets none.
//! - downgrade (S9b): a third run installs version 1 again: refused INSTALL_FAILED_VERSION_DOWNGRADE (atld says so), yet the installed version 2 stays, its activity runs and atld exits 0.
//!
//! Failure prints the apps' and the services' log tails; the counters are printed always.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime, counter_lines, set_default_route};
use crate::manifest;
use crate::wait::{LOOP_POLL, Miss, Wait, settle_for, until, until_child};

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
/// The sender's DONE line (Main.java: the DONE receiver, on the looper the PING receiver shares): the absence checks are judged once it is logged, see [`once_before_done`]. A fixed wait would pass a duplicate a starved host delivers late.
const DONE_RECEIVED: &str = "AtlBcast: done received";
/// Reinstall run only: the fixed wait after the receiver's MY_PACKAGE_REPLACED, before the sender's silence is read. It stays a sleep: the sender is not launched in this run (`launch: None`), so no fence on its queue exists yet.
const REINSTALL_SETTLE: Duration = Duration::from_secs(3);
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
// the S9 boot rows: BootRecv / BootSvc log lines (testdata receiver and sender), and the skip counter of the opt-in rule (broadcasts_am.rs dispatch_skip)
const BOOT_LOCKED: &str = "AtlBcast: boot received android.intent.action.LOCKED_BOOT_COMPLETED";
const BOOT_BOOT: &str = "AtlBcast: boot received android.intent.action.BOOT_COMPLETED";
const BOOT_MY_REPLACED: &str = "AtlBcast: boot received android.intent.action.MY_PACKAGE_REPLACED";
const BOOT_SERVICE_STARTED: &str = "AtlBcast: boot service started";
const BOOT_SERVICE_REFUSED: &str = "AtlBcast: boot service refused";
const BOOT_SKIPPED: &str = "boot broadcast skipped";
/// The counter key of boot_refusal's directBootAware refusal (its reason's first ':' part).
const BOOT_LOCKED_REFUSED: &str = "LOCKED_BOOT_COMPLETED";
/// D2 clause 2's counters (broadcasts_am.rs first_start_boot_locked, release_held_launches_locked).
const FIRST_START_SENT: &str = "boot broadcasts at first process start";
const LAUNCH_RELEASED: &str = "launch released after first-start boot broadcasts";
/// install.rs change_of's refusal text (PackageManagerServiceUtils.checkDowngrade :1427-1429), version 1 over the recorded 2.
const DOWNGRADE_REFUSED: &str =
    "INSTALL_FAILED_VERSION_DOWNGRADE: Update version code 1 is older than current 2";
/// atld's user line for that refusal (main.rs Host::reinstate): both versionCodes, and that the installed one runs.
const DOWNGRADE_USER_LINE: &str = "atld: install com.example.atlbcastrecv: refused INSTALL_FAILED_VERSION_DOWNGRADE: versionCode 1 is older than the installed 2; the installed versionCode 2 runs";
/// The receiver's Show activity in the downgrade run: the installed version 2's code runs.
const KEPT_V2_RUNS: &str = "AtlBcast: activity onCreate versionCode=2";

/// Copies the fixture tree `from` to `to` (the reinstall row's version-2 receiver is the receiver fixture with its versionCode changed; audio_app.rs's ducker is its fixture under another package).
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn count(text: &str, marker: &str) -> usize {
    text.lines().filter(|l| l.contains(marker)).count()
}

/// Absence judgement on the sender's log: DONE is logged and exactly one runtime PING precedes it. A PING after DONE is not judged: DONE is FIFO behind any PING queued before it on the sender's looper (broadcast_queue.rs:4-6), so nothing in flight can precede it.
fn once_before_done(sender_log: &str) -> bool {
    sender_log
        .split_once(DONE_RECEIVED)
        .is_some_and(|(before, _)| count(before, RUNTIME) == 1)
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
    let mut apk_paths: Vec<PathBuf> = Vec::new();
    for (fixture, dir) in [(SENDER, "sender"), (RECEIVER, "receiver")] {
        let apk = rt
            .apk
            .build(&root.join(fixture), &work.join(dir), &[], None, None)?;
        let m = manifest::read(&rt.apk.aapt2, &apk)?;
        println!(
            "broadcast: built {} ({}, {} receivers)",
            apk.display(),
            m.name,
            m.receiver_count
        );
        apk_paths.push(apk.clone());
        packages.push(Package::base(apk));
        names.push(m.name);
    }
    let (sender, receiver) = (names[0].clone(), names[1].clone());
    // the device's data root, kept across the boot run and the reinstall run (atld's install record, the package list and the opt-in); the connectivity run below stays fresh
    let data = work.join("data");
    let _ = std::fs::remove_dir_all(&data);
    let kept_runtime = |autostart: &[&str]| -> Result<Runtime> {
        Ok(Runtime::new(
            root,
            "broadcast",
            &args.host,
            args.out.as_deref(),
            args.java_out.as_deref(),
            "broadcast",
            args.verbose,
        )?
        .with_empty_process_grace(EMPTY_GRACE_MS)
        .with_data_root(&data)
        .with_autostart(autostart))
    };
    // the boot run has no opt-in (decision D2's default): every boot receiver is skipped. The opt-in run is the reinstall run, after the restriction rows: a boot grant would
    // make the receiver's uid active for 20 s here (AOSP-correct), so the restricted-start rows must not share its window
    let kept = kept_runtime(&[])?;
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(sender.clone()),
        packages,
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    let mut atld = kept.start_atld(&run)?;
    let read = atl_diag::logfiles::read;
    let (sender_log, receiver_log) = (run.log(&sender), run.log(&receiver));
    let start = Instant::now();
    let services_log = run.log("services");
    // the PING's line, seen at a poll: the headless end counts only after it (the receiver finishes after its PING)
    let ping_seen = Cell::new(false);
    // the headless end follows the receiver's finish by EMPTY_GRACE_MS: its line (LIMIT caps the wait)
    let end_line = format!("process {receiver} ended: empty headless (grace {EMPTY_GRACE_MS} ms)");
    // the run ends once the sender's DONE (the fence of once_before_done), the headless end and the alarm chain's last step are logged.
    // The alarm chain (Recv.java ALARM_GRANT) ends only once AlarmSvc started, so the grant is inside the run
    let run_end = || {
        if !ping_seen.get() && read(&receiver_log).contains(PING_SENT) {
            ping_seen.set(true);
        }
        ping_seen.get()
            && read(&services_log).contains(&end_line)
            && read(&sender_log).contains(DONE_RECEIVED)
            && read(&receiver_log).contains(ALARM_SERVICE_STARTED)
    };
    // a run end that runs out is not judged here: the checks below read the logs as they are. atld's death is a verdict line
    let ended = match until_child(
        &Wait::logs(
            "broadcast-run-end",
            "no done, headless end or alarm line",
            LIMIT,
            LOOP_POLL,
            &run_end,
        ),
        &mut atld,
    ) {
        Ok(_) | Err(Miss::Late { .. }) => None,
        Err(Miss::Exited { status, .. }) => Some(status),
        Err(miss) => anyhow::bail!(miss.text()),
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
    // the reinstall row (plan S9): version 2 of the receiver over the same data root, the same package order (uids) and opt-in. atld installs at start, so this second run is the upgrade
    let receiver_v2 = {
        let v2 = work.join("receiver-v2");
        let _ = std::fs::remove_dir_all(&v2);
        copy_tree(&root.join(RECEIVER), &v2)?;
        let manifest_file = v2.join("AndroidManifest.xml");
        let text = std::fs::read_to_string(&manifest_file)?;
        let v2_text = text
            .replace("android:versionCode=\"1\"", "android:versionCode=\"2\"")
            .replace("\n        BOOT_RECEIVER_V2 ", " -->\n        ")
            .replace(" BOOT_RECEIVER_V2_END -->", "");
        anyhow::ensure!(
            v2_text.contains("android:versionCode=\"2\"") && !v2_text.contains("BOOT_RECEIVER_V2"),
            "{}: the version-2 edits did not apply",
            manifest_file.display()
        );
        std::fs::write(&manifest_file, v2_text)?;
        rt.apk
            .build(&v2, &work.join("receiver-v2-build"), &[], None, None)?
    };
    let run2 = Run {
        root: work.join("root2"),
        scratch: work.join("scratch2"),
        launch: None,
        packages: vec![
            Package::base(apk_paths[0].clone()),
            Package::base(receiver_v2),
        ],
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    let opt_in = kept_runtime(&[receiver.as_str()])?;
    let mut atld2 = opt_in.start_atld(&run2)?;
    let (r2_log, s2_log) = (run2.log(&receiver), run2.log(&sender));
    let start2 = Instant::now();
    // the receiver's MY_PACKAGE_REPLACED, then the sender's silence is read after REINSTALL_SETTLE (what is left of LIMIT at most): a late delivery lands before it.
    // The settle stays a timed wait (see REINSTALL_SETTLE): no fence on the sender's queue exists in this run
    let replaced = || read(&r2_log).contains(BOOT_MY_REPLACED);
    let mut atld2_ended = false;
    match until_child(
        &Wait::logs(
            "reinstall-replaced",
            "no MY_PACKAGE_REPLACED line",
            LIMIT,
            LOOP_POLL,
            &replaced,
        ),
        &mut atld2,
    ) {
        Ok(at) => {
            let left = LIMIT.saturating_sub(at.saturating_duration_since(start2));
            match settle_for("reinstall-settle", REINSTALL_SETTLE.min(left), &mut atld2) {
                Ok(()) => {}
                Err(Miss::Exited { .. }) => atld2_ended = true,
                Err(miss) => anyhow::bail!(miss.text()),
            }
        }
        Err(Miss::Exited { .. }) => atld2_ended = true,
        Err(Miss::Late { .. }) => {}
        Err(miss) => anyhow::bail!(miss.text()),
    }
    if !atld2_ended {
        let _ = atld2.kill();
        atld2.wait()?;
    }
    println!(
        "broadcast: reinstall run {:.1?} (atld start to the receiver's MY_PACKAGE_REPLACED, plus REINSTALL_SETTLE)",
        start2.elapsed()
    );
    let (s2, r2) = (read(&s2_log), read(&r2_log));
    let services2 = read(&run2.log("services"));
    for l in counter_lines(&services2) {
        println!("broadcast: reinstall counter {l}");
    }
    // the downgrade row: version 1 of the receiver again over the same data root, recorded at 2. The install refuses that session (INSTALL_FAILED_VERSION_DOWNGRADE) and the installed version 2
    // stays and runs: the receiver is launched, logs its versionCode and finishes, and atld ends with its last app
    let run3 = Run {
        root: work.join("root3"),
        scratch: work.join("scratch3"),
        launch: Some(receiver.clone()),
        packages: vec![
            Package::base(apk_paths[0].clone()),
            Package::base(apk_paths[1].clone()),
        ],
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    let start3 = Instant::now();
    let mut atld3 = kept_runtime(&[])?
        .with_exit_with_last_app()
        .start_atld(&run3)?;
    let ended3 = loop {
        if let Some(s) = atld3.try_wait()? {
            break Some(s);
        }
        if start3.elapsed() > LIMIT {
            let _ = atld3.kill();
            atld3.wait()?;
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    println!(
        "broadcast: downgrade run {:.1?} (atld start to its exit)",
        start3.elapsed()
    );
    let services3 = read(&run3.log("services"));
    let atld3_log = read(&run3.atld_log());
    let r3 = read(&run3.log(&receiver));
    let refused3 = services3.contains(DOWNGRADE_REFUSED) && atld3_log.contains(DOWNGRADE_USER_LINE);
    let kept3 = services3.contains(&format!("services: install {receiver} version 2 Unchanged"))
        && r3.contains(KEPT_V2_RUNS);
    if !(refused3 && kept3) {
        println!(
            "---- downgrade run atld log (last {TAIL})\n{}\n---- services log (last {TAIL})\n{}",
            tail(&atld3_log, TAIL),
            tail(&services3, TAIL)
        );
    }
    let mut checks = vec![
        (
            "runtime receiver got its package's broadcast once",
            once_before_done(&s),
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
            ping_seen.get() && counted(EXPORTED_DENIAL) && once_before_done(&s),
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
        (
            "the first install recorded both packages as new (the services' install reports)",
            services.contains("services: install com.example.atlbcast version 1 New")
                && services.contains("services: install com.example.atlbcastrecv version 1 New"),
        ),
        (
            "with no opt-in, the start-time boot broadcasts were skipped for the sender (counter), and the receiver app's version 1 (no boot receiver) got none",
            !r.contains(BOOT_LOCKED) && !r.contains(BOOT_BOOT) && counted(BOOT_SKIPPED),
        ),
        (
            "LOCKED_BOOT_COMPLETED was refused for the receiver that is not directBootAware (counter)",
            counted(BOOT_LOCKED_REFUSED) && !s.contains(BOOT_LOCKED),
        ),
        (
            "the sender, not opted in, got BOOT_COMPLETED at its first process start, once, ahead of its activity (D2 clause 2; its launch was held and released, counters)",
            count(&s, BOOT_BOOT) == 1
                && matches!((s.find(BOOT_BOOT), s.find(STICKY_RETURNED)), (Some(b), Some(a)) if b < a)
                && counted(FIRST_START_SENT)
                && counted(LAUNCH_RELEASED),
        ),
        (
            "BOOT_COMPLETED reached the opted-in receiver app in the opt-in run",
            r2.contains(BOOT_BOOT),
        ),
        (
            "LOCKED_BOOT_COMPLETED reached it before BOOT_COMPLETED (the in-process resultTo chain)",
            matches!((r2.find(BOOT_LOCKED), r2.find(BOOT_BOOT)), (Some(l), Some(b)) if l < b),
        ),
        (
            "the receiver started by BOOT_COMPLETED started its service inside the temp allowlist",
            r2.contains(BOOT_SERVICE_STARTED) && !r2.contains(BOOT_SERVICE_REFUSED),
        ),
        (
            "the package not opted in got neither boot broadcast in the opt-in run, and each skip is counted",
            !s2.contains(BOOT_LOCKED)
                && !s2.contains(BOOT_BOOT)
                && counter_lines(&services2)
                    .iter()
                    .any(|l| l.contains(BOOT_SKIPPED)),
        ),
        (
            "a reinstall with a higher versionCode reached the receiver app's MY_PACKAGE_REPLACED",
            r2.contains(BOOT_MY_REPLACED),
        ),
        (
            "the reinstall classified the receiver Updated from version 1 and the same-version sender Unchanged",
            services2.contains(
                "services: install com.example.atlbcastrecv version 2 Updated { from: 1 }",
            ) && services2.contains("services: install com.example.atlbcast version 1 Unchanged"),
        ),
        (
            "a same-version reinstall sent the sender no MY_PACKAGE_REPLACED",
            !s2.contains(BOOT_MY_REPLACED),
        ),
        (
            "a lower versionCode over the recorded one was refused INSTALL_FAILED_VERSION_DOWNGRADE, said in atld's one line naming both versionCodes",
            refused3,
        ),
        (
            "after the refusal the installed version 2 stayed (Unchanged), ran (its activity's onCreate) and atld exited 0 with its last app",
            kept3 && ended3.is_some_and(|s| s.success()),
        ),
    ];
    checks.extend(connectivity_checks(root, &mut rt, &work)?);
    checks.extend(internet_checks(&s));
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

/// The sender's INTERNET rows (Main.java internetRows; the sender has no INTERNET): a loopback ServerSocket fails at socket() with EPERM, a name lookup throws SecurityException
/// ("missing INTERNET permission?", libcore Inet6AddressImpl), and localhost still resolves from the hosts file. A row whose line is absent fails.
fn internet_checks(sender_log: &str) -> Vec<(&'static str, bool)> {
    let value = |key: &str| {
        sender_log
            .lines()
            .find_map(|l| l.split_once(&format!("net {key}=")).map(|(_, v)| v))
            .unwrap_or("")
    };
    vec![
        (
            "net: a loopback ServerSocket without INTERNET fails EPERM at socket()",
            value("server-socket").contains("EPERM"),
        ),
        (
            "net: a name lookup without INTERNET throws SecurityException (missing INTERNET permission)",
            value("dns").starts_with("SecurityException")
                && value("dns").contains("missing INTERNET permission"),
        ),
        (
            "net: localhost resolves to 127.0.0.1 without INTERNET",
            value("localhost") == "127.0.0.1",
        ),
    ]
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
    let app = run.log(&package);
    let read = atl_diag::logfiles::read;
    // waits up to NET_STEP for the app's log to hold `marker`: one declared wait per step (its record); a late step is false
    let seen = |name: &'static str, marker: &str| -> bool {
        let held = || read(&app).contains(marker);
        until(&Wait::logs(
            name,
            format!("no {marker} line"),
            NET_STEP,
            LOOP_POLL,
            &held,
        ))
        .is_ok()
    };
    // each step needs the one before it: a failed step leaves the later ones unchecked (false)
    let start_seen = seen("net-start", NET_START);
    let sticky_start = start_seen && seen("net-sticky-start", NET_STICKY_START);
    let lost_seen =
        start_seen && set_default_route(atld.id(), false).is_ok() && seen("net-lost", NET_LOST);
    let sticky_lost = lost_seen && seen("net-sticky-lost", NET_STICKY_LOST);
    let again_seen =
        lost_seen && set_default_route(atld.id(), true).is_ok() && seen("net-again", NET_AGAIN);
    let sticky_again = again_seen && seen("net-sticky-again", NET_STICKY_AGAIN);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorded sender log: the lines in the order the sender's runtime receiver logged them.
    fn log(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn own_ping_then_done_passes() {
        assert!(once_before_done(&log(&[RUNTIME, DONE_RECEIVED])));
    }

    /// The lines an app without INTERNET logs (AOSP's answers, as the sender's Main.java prints them) pass every row; an app whose sockets are open fails the first two.
    #[test]
    fn internet_rows_pass_only_the_refusals_of_an_app_without_internet() {
        let refused = log(&[
            "AtlBcast: net server-socket=java.net.SocketException: socket failed: EPERM (Operation not permitted)",
            "AtlBcast: net dns=SecurityException: Permission denied (missing INTERNET permission?)",
            "AtlBcast: net localhost=127.0.0.1",
        ]);
        assert!(internet_checks(&refused).iter().all(|(_, ok)| *ok));
        let open = log(&[
            "AtlBcast: net server-socket=ok",
            "AtlBcast: net dns=UnknownHostException",
            "AtlBcast: net localhost=127.0.0.1",
        ]);
        let failed = internet_checks(&open).iter().filter(|(_, ok)| !ok).count();
        assert_eq!(failed, 2);
        assert!(internet_checks("").iter().all(|(_, ok)| !ok));
    }

    #[test]
    fn duplicate_ping_before_done_is_red_at_done() {
        let lines = [RUNTIME, RUNTIME, DONE_RECEIVED];
        assert!(!once_before_done(&log(&lines)));
    }

    #[test]
    fn no_done_is_not_judged_as_absence() {
        assert!(!once_before_done(&log(&[RUNTIME])));
    }

    #[test]
    fn ping_after_done_is_not_judged() {
        let lines = [RUNTIME, DONE_RECEIVED, RUNTIME];
        assert!(once_before_done(&log(&lines)));
    }
}
