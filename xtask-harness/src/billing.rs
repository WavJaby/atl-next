// SPDX-License-Identifier: MIT
//! `xtask test billing`: the billing-unavailable desktop adaptation (M62 D1). Fixture xtask-harness/testdata/billing, a Play Billing client's first steps: it sees the store's billing service only
//! through its `<queries><intent>` (the responder has no forceQueryable), binds it with the implicit Intent + setPackage, and asks isBillingSupported / isBillingSupportedExtraParams.
//! - default (setting on, `Runtime::with_billing_unavailable`): the query finds the one service, the bind connects to it, both calls answer 3 (BILLING_UNAVAILABLE), an unlisted code is unanswered;
//! - `--off` (no vending package, exact AOSP without Play): the query is empty, bindService answers false and nothing connects.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime};

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
    /// The setting off: the run places no vending package, so the client must see AOSP without Play (empty query, failed bind).
    #[arg(long)]
    off: bool,
    /// Print the app's whole log.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/billing";
const PACKAGE: &str = "com.example.atlbilling";
/// A cold start (ART without a boot image interprets framework.jar's class init), as `xtask test app`'s limit.
const LIMIT: Duration = Duration::from_secs(180);
const TAIL: usize = 30;
const DONE: &str = "AtlBilling: done";
const CONNECTED: &str = "AtlBilling: connected ";

/// The lines each setting must show. On: one resolved service, a connection to the responder's class, 3 = BillingResponseCode.BILLING_UNAVAILABLE for codes 1 and 10, code 2 unanswered.
/// Off: AOSP without the package -- queryIntentServices empty, bindService false (bindServiceLocked found nothing).
fn want(off: bool) -> &'static str {
    if off {
        "AtlBilling: query 0 bind false"
    } else {
        "AtlBilling: query 1 bind true"
    }
}
const ANSWER: &str = "AtlBilling: connected com.android.vending.billing.InAppBillingService supported 3 extra 3 other false";

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let _ = Layout::of(root)?;
    let rt = Runtime::new(
        root,
        "billing",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "billing",
        args.verbose,
    )?;
    let rt = if args.off {
        rt
    } else {
        rt.with_billing_unavailable()?
    };
    let work = rt.out.join("billing");
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("app"), &[], None, None)?;
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(PACKAGE.to_owned()),
        packages: vec![Package::base(apk)],
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    let mut atld = rt.start_atld(&run)?;
    let read = atl_diag::logfiles::read;
    let log = run.log(PACKAGE);
    let start = Instant::now();
    let ended = loop {
        if read(&log).contains(DONE) || start.elapsed() > LIMIT {
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
    let text = read(&log);
    if args.verbose {
        println!("---- app log\n{text}");
    }
    let setting = if args.off { "off" } else { "on" };
    let checks = [
        ("query and bind", text.contains(want(args.off))),
        (
            "the connection's answers",
            if args.off {
                !text.contains(CONNECTED)
            } else {
                text.contains(ANSWER)
            },
        ),
    ];
    let mut whys = Vec::new();
    for (what, ok) in checks {
        println!(
            "billing: {} {what} (setting {setting})",
            if ok { "pass" } else { "FAIL" }
        );
        if !ok {
            whys.push(what.to_owned());
        }
    }
    if whys.is_empty() {
        println!(
            "billing: pass (setting {setting}, {} checks in {:.1?})",
            checks.len(),
            start.elapsed()
        );
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(e) = ended {
        whys.push(format!("atld exited {e}"));
    }
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL})\n{}", tail(&text, TAIL));
    println!(
        "---- services log (last {TAIL})\n{}",
        tail(&read(&run.log("services")), TAIL)
    );
    println!("{}", crate::verdict::gate("billing", &whys));
    Ok(ExitCode::FAILURE)
}
