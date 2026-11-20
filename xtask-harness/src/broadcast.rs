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
//!
//! Failure prints the apps' and the services' log tails; the counters are printed always.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime, counter_lines};
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
/// BroadcastController.java:457-460's message, callerPackage first.
const UNFLAGGED_DENIED: &str = "AtlBcast: unflagged register denied: com.example.atlbcast: One of RECEIVER_EXPORTED or RECEIVER_NOT_EXPORTED should be specified when a receiver isn't being registered exclusively for system broadcasts";
/// Counter names the services count (activity.rs): the cold start of a manifest receiver's process, and the skip of an unexported registered receiver.
const COLD_START: &str = "broadcast cold start";
const EXPORTED_DENIAL: &str = "Exported Denial";

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn count(text: &str, marker: &str) -> usize {
    text.lines().filter(|l| l.contains(marker)).count()
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let _ = Layout::of(root)?;
    let rt = Runtime::new(
        root,
        "broadcast",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "broadcast-art",
        args.verbose,
    )?;
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
    let ended = loop {
        if ping_seen.is_none() && read(&receiver_log).contains(PING_SENT) {
            ping_seen = Some(Instant::now());
        }
        if ping_seen.is_some_and(|t| t.elapsed() > SETTLE) || start.elapsed() > LIMIT {
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
    let checks = [
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
            s.lines().any(|l| l.ends_with(&system_level)) && !s.lines().any(|l| l.ends_with(FAKE_LEVEL)),
        ),
        (
            "a receiver of only protected actions registered without an export flag",
            s.contains(PROTECTED_ONLY_REGISTERED),
        ),
        (
            "a receiver of an unprotected action without an export flag threw SecurityException",
            s.contains(UNFLAGGED_DENIED),
        ),
    ];
    let mut whys = Vec::new();
    for (what, ok) in checks {
        println!("broadcast: {} {what}", if ok { "pass" } else { "FAIL" });
        if !ok {
            whys.push(what.to_owned());
        }
    }
    if whys.is_empty() {
        println!(
            "broadcast: pass ({} checks in {:.1?})",
            checks.len(),
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
